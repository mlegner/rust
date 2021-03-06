//! This file declares the `ScopeTree` type, which describes
//! the parent links in the region hierarchy.
//!
//! For more information about how MIR-based region-checking works,
//! see the [rustc guide].
//!
//! [rustc guide]: https://rust-lang.github.io/rustc-guide/mir/borrowck.html

use crate::ich::{NodeIdHashingMode, StableHashingContext};
use crate::ty::{self, DefIdTree, TyCtxt};
use rustc_hir as hir;
use rustc_hir::def_id::DefId;
use rustc_hir::Node;

use rustc_data_structures::fx::FxHashMap;
use rustc_data_structures::stable_hasher::{HashStable, StableHasher};
use rustc_macros::HashStable;
use rustc_span::{Span, DUMMY_SP};

use std::fmt;

/// Represents a statically-describable scope that can be used to
/// bound the lifetime/region for values.
///
/// `Node(node_id)`: Any AST node that has any scope at all has the
/// `Node(node_id)` scope. Other variants represent special cases not
/// immediately derivable from the abstract syntax tree structure.
///
/// `DestructionScope(node_id)` represents the scope of destructors
/// implicitly-attached to `node_id` that run immediately after the
/// expression for `node_id` itself. Not every AST node carries a
/// `DestructionScope`, but those that are `terminating_scopes` do;
/// see discussion with `ScopeTree`.
///
/// `Remainder { block, statement_index }` represents
/// the scope of user code running immediately after the initializer
/// expression for the indexed statement, until the end of the block.
///
/// So: the following code can be broken down into the scopes beneath:
///
/// ```text
/// let a = f().g( 'b: { let x = d(); let y = d(); x.h(y)  }   ) ;
///
///                                                              +-+ (D12.)
///                                                        +-+       (D11.)
///                                              +---------+         (R10.)
///                                              +-+                  (D9.)
///                                   +----------+                    (M8.)
///                                 +----------------------+          (R7.)
///                                 +-+                               (D6.)
///                      +----------+                                 (M5.)
///                    +-----------------------------------+          (M4.)
///         +--------------------------------------------------+      (M3.)
///         +--+                                                      (M2.)
/// +-----------------------------------------------------------+     (M1.)
///
///  (M1.): Node scope of the whole `let a = ...;` statement.
///  (M2.): Node scope of the `f()` expression.
///  (M3.): Node scope of the `f().g(..)` expression.
///  (M4.): Node scope of the block labeled `'b:`.
///  (M5.): Node scope of the `let x = d();` statement
///  (D6.): DestructionScope for temporaries created during M5.
///  (R7.): Remainder scope for block `'b:`, stmt 0 (let x = ...).
///  (M8.): Node scope of the `let y = d();` statement.
///  (D9.): DestructionScope for temporaries created during M8.
/// (R10.): Remainder scope for block `'b:`, stmt 1 (let y = ...).
/// (D11.): DestructionScope for temporaries and bindings from block `'b:`.
/// (D12.): DestructionScope for temporaries created during M1 (e.g., f()).
/// ```
///
/// Note that while the above picture shows the destruction scopes
/// as following their corresponding node scopes, in the internal
/// data structures of the compiler the destruction scopes are
/// represented as enclosing parents. This is sound because we use the
/// enclosing parent relationship just to ensure that referenced
/// values live long enough; phrased another way, the starting point
/// of each range is not really the important thing in the above
/// picture, but rather the ending point.
//
// FIXME(pnkfelix): this currently derives `PartialOrd` and `Ord` to
// placate the same deriving in `ty::FreeRegion`, but we may want to
// actually attach a more meaningful ordering to scopes than the one
// generated via deriving here.
#[derive(
    Clone,
    PartialEq,
    PartialOrd,
    Eq,
    Ord,
    Hash,
    Copy,
    RustcEncodable,
    RustcDecodable,
    HashStable
)]
pub struct Scope {
    pub id: hir::ItemLocalId,
    pub data: ScopeData,
}

impl fmt::Debug for Scope {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.data {
            ScopeData::Node => write!(fmt, "Node({:?})", self.id),
            ScopeData::CallSite => write!(fmt, "CallSite({:?})", self.id),
            ScopeData::Arguments => write!(fmt, "Arguments({:?})", self.id),
            ScopeData::Destruction => write!(fmt, "Destruction({:?})", self.id),
            ScopeData::Remainder(fsi) => write!(
                fmt,
                "Remainder {{ block: {:?}, first_statement_index: {}}}",
                self.id,
                fsi.as_u32(),
            ),
        }
    }
}

#[derive(
    Clone,
    PartialEq,
    PartialOrd,
    Eq,
    Ord,
    Hash,
    Debug,
    Copy,
    RustcEncodable,
    RustcDecodable,
    HashStable
)]
pub enum ScopeData {
    Node,

    /// Scope of the call-site for a function or closure
    /// (outlives the arguments as well as the body).
    CallSite,

    /// Scope of arguments passed to a function or closure
    /// (they outlive its body).
    Arguments,

    /// Scope of destructors for temporaries of node-id.
    Destruction,

    /// Scope following a `let id = expr;` binding in a block.
    Remainder(FirstStatementIndex),
}

rustc_index::newtype_index! {
    /// Represents a subscope of `block` for a binding that is introduced
    /// by `block.stmts[first_statement_index]`. Such subscopes represent
    /// a suffix of the block. Note that each subscope does not include
    /// the initializer expression, if any, for the statement indexed by
    /// `first_statement_index`.
    ///
    /// For example, given `{ let (a, b) = EXPR_1; let c = EXPR_2; ... }`:
    ///
    /// * The subscope with `first_statement_index == 0` is scope of both
    ///   `a` and `b`; it does not include EXPR_1, but does include
    ///   everything after that first `let`. (If you want a scope that
    ///   includes EXPR_1 as well, then do not use `Scope::Remainder`,
    ///   but instead another `Scope` that encompasses the whole block,
    ///   e.g., `Scope::Node`.
    ///
    /// * The subscope with `first_statement_index == 1` is scope of `c`,
    ///   and thus does not include EXPR_2, but covers the `...`.
    pub struct FirstStatementIndex {
        derive [HashStable]
    }
}

// compilation error if size of `ScopeData` is not the same as a `u32`
static_assert_size!(ScopeData, 4);

impl Scope {
    /// Returns a item-local ID associated with this scope.
    ///
    /// N.B., likely to be replaced as API is refined; e.g., pnkfelix
    /// anticipates `fn entry_node_id` and `fn each_exit_node_id`.
    pub fn item_local_id(&self) -> hir::ItemLocalId {
        self.id
    }

    pub fn hir_id(&self, scope_tree: &ScopeTree) -> hir::HirId {
        match scope_tree.root_body {
            Some(hir_id) => hir::HirId { owner: hir_id.owner, local_id: self.item_local_id() },
            None => hir::DUMMY_HIR_ID,
        }
    }

    /// Returns the span of this `Scope`. Note that in general the
    /// returned span may not correspond to the span of any `NodeId` in
    /// the AST.
    pub fn span(&self, tcx: TyCtxt<'_>, scope_tree: &ScopeTree) -> Span {
        let hir_id = self.hir_id(scope_tree);
        if hir_id == hir::DUMMY_HIR_ID {
            return DUMMY_SP;
        }
        let span = tcx.hir().span(hir_id);
        if let ScopeData::Remainder(first_statement_index) = self.data {
            if let Node::Block(ref blk) = tcx.hir().get(hir_id) {
                // Want span for scope starting after the
                // indexed statement and ending at end of
                // `blk`; reuse span of `blk` and shift `lo`
                // forward to end of indexed statement.
                //
                // (This is the special case aluded to in the
                // doc-comment for this method)

                let stmt_span = blk.stmts[first_statement_index.index()].span;

                // To avoid issues with macro-generated spans, the span
                // of the statement must be nested in that of the block.
                if span.lo() <= stmt_span.lo() && stmt_span.lo() <= span.hi() {
                    return Span::new(stmt_span.lo(), span.hi(), span.ctxt());
                }
            }
        }
        span
    }
}

pub type ScopeDepth = u32;

/// The region scope tree encodes information about region relationships.
#[derive(Default, Debug)]
pub struct ScopeTree {
    /// If not empty, this body is the root of this region hierarchy.
    pub root_body: Option<hir::HirId>,

    /// The parent of the root body owner, if the latter is an
    /// an associated const or method, as impls/traits can also
    /// have lifetime parameters free in this body.
    pub root_parent: Option<hir::HirId>,

    /// Maps from a scope ID to the enclosing scope id;
    /// this is usually corresponding to the lexical nesting, though
    /// in the case of closures the parent scope is the innermost
    /// conditional expression or repeating block. (Note that the
    /// enclosing scope ID for the block associated with a closure is
    /// the closure itself.)
    pub parent_map: FxHashMap<Scope, (Scope, ScopeDepth)>,

    /// Maps from a variable or binding ID to the block in which that
    /// variable is declared.
    var_map: FxHashMap<hir::ItemLocalId, Scope>,

    /// Maps from a `NodeId` to the associated destruction scope (if any).
    destruction_scopes: FxHashMap<hir::ItemLocalId, Scope>,

    /// `rvalue_scopes` includes entries for those expressions whose
    /// cleanup scope is larger than the default. The map goes from the
    /// expression ID to the cleanup scope id. For rvalues not present in
    /// this table, the appropriate cleanup scope is the innermost
    /// enclosing statement, conditional expression, or repeating
    /// block (see `terminating_scopes`).
    /// In constants, None is used to indicate that certain expressions
    /// escape into 'static and should have no local cleanup scope.
    rvalue_scopes: FxHashMap<hir::ItemLocalId, Option<Scope>>,

    /// Encodes the hierarchy of fn bodies. Every fn body (including
    /// closures) forms its own distinct region hierarchy, rooted in
    /// the block that is the fn body. This map points from the ID of
    /// that root block to the ID of the root block for the enclosing
    /// fn, if any. Thus the map structures the fn bodies into a
    /// hierarchy based on their lexical mapping. This is used to
    /// handle the relationships between regions in a fn and in a
    /// closure defined by that fn. See the "Modeling closures"
    /// section of the README in infer::region_constraints for
    /// more details.
    closure_tree: FxHashMap<hir::ItemLocalId, hir::ItemLocalId>,

    /// If there are any `yield` nested within a scope, this map
    /// stores the `Span` of the last one and its index in the
    /// postorder of the Visitor traversal on the HIR.
    ///
    /// HIR Visitor postorder indexes might seem like a peculiar
    /// thing to care about. but it turns out that HIR bindings
    /// and the temporary results of HIR expressions are never
    /// storage-live at the end of HIR nodes with postorder indexes
    /// lower than theirs, and therefore don't need to be suspended
    /// at yield-points at these indexes.
    ///
    /// For an example, suppose we have some code such as:
    /// ```rust,ignore (example)
    ///     foo(f(), yield y, bar(g()))
    /// ```
    ///
    /// With the HIR tree (calls numbered for expository purposes)
    /// ```
    ///     Call#0(foo, [Call#1(f), Yield(y), Call#2(bar, Call#3(g))])
    /// ```
    ///
    /// Obviously, the result of `f()` was created before the yield
    /// (and therefore needs to be kept valid over the yield) while
    /// the result of `g()` occurs after the yield (and therefore
    /// doesn't). If we want to infer that, we can look at the
    /// postorder traversal:
    /// ```plain,ignore
    ///     `foo` `f` Call#1 `y` Yield `bar` `g` Call#3 Call#2 Call#0
    /// ```
    ///
    /// In which we can easily see that `Call#1` occurs before the yield,
    /// and `Call#3` after it.
    ///
    /// To see that this method works, consider:
    ///
    /// Let `D` be our binding/temporary and `U` be our other HIR node, with
    /// `HIR-postorder(U) < HIR-postorder(D)` (in our example, U would be
    /// the yield and D would be one of the calls). Let's show that
    /// `D` is storage-dead at `U`.
    ///
    /// Remember that storage-live/storage-dead refers to the state of
    /// the *storage*, and does not consider moves/drop flags.
    ///
    /// Then:
    ///     1. From the ordering guarantee of HIR visitors (see
    ///     `rustc::hir::intravisit`), `D` does not dominate `U`.
    ///     2. Therefore, `D` is *potentially* storage-dead at `U` (because
    ///     we might visit `U` without ever getting to `D`).
    ///     3. However, we guarantee that at each HIR point, each
    ///     binding/temporary is always either always storage-live
    ///     or always storage-dead. This is what is being guaranteed
    ///     by `terminating_scopes` including all blocks where the
    ///     count of executions is not guaranteed.
    ///     4. By `2.` and `3.`, `D` is *statically* storage-dead at `U`,
    ///     QED.
    ///
    /// This property ought to not on (3) in an essential way -- it
    /// is probably still correct even if we have "unrestricted" terminating
    /// scopes. However, why use the complicated proof when a simple one
    /// works?
    ///
    /// A subtle thing: `box` expressions, such as `box (&x, yield 2, &y)`. It
    /// might seem that a `box` expression creates a `Box<T>` temporary
    /// when it *starts* executing, at `HIR-preorder(BOX-EXPR)`. That might
    /// be true in the MIR desugaring, but it is not important in the semantics.
    ///
    /// The reason is that semantically, until the `box` expression returns,
    /// the values are still owned by their containing expressions. So
    /// we'll see that `&x`.
    pub yield_in_scope: FxHashMap<Scope, YieldData>,

    /// The number of visit_expr and visit_pat calls done in the body.
    /// Used to sanity check visit_expr/visit_pat call count when
    /// calculating generator interiors.
    pub body_expr_count: FxHashMap<hir::BodyId, usize>,
}

#[derive(Debug, Copy, Clone, RustcEncodable, RustcDecodable, HashStable)]
pub struct YieldData {
    /// The `Span` of the yield.
    pub span: Span,
    /// The number of expressions and patterns appearing before the `yield` in the body plus one.
    pub expr_and_pat_count: usize,
    pub source: hir::YieldSource,
}

impl<'tcx> ScopeTree {
    pub fn record_scope_parent(&mut self, child: Scope, parent: Option<(Scope, ScopeDepth)>) {
        debug!("{:?}.parent = {:?}", child, parent);

        if let Some(p) = parent {
            let prev = self.parent_map.insert(child, p);
            assert!(prev.is_none());
        }

        // Record the destruction scopes for later so we can query them.
        if let ScopeData::Destruction = child.data {
            self.destruction_scopes.insert(child.item_local_id(), child);
        }
    }

    pub fn each_encl_scope<E>(&self, mut e: E)
    where
        E: FnMut(Scope, Scope),
    {
        for (&child, &parent) in &self.parent_map {
            e(child, parent.0)
        }
    }

    pub fn each_var_scope<E>(&self, mut e: E)
    where
        E: FnMut(&hir::ItemLocalId, Scope),
    {
        for (child, &parent) in self.var_map.iter() {
            e(child, parent)
        }
    }

    pub fn opt_destruction_scope(&self, n: hir::ItemLocalId) -> Option<Scope> {
        self.destruction_scopes.get(&n).cloned()
    }

    /// Records that `sub_closure` is defined within `sup_closure`. These IDs
    /// should be the ID of the block that is the fn body, which is
    /// also the root of the region hierarchy for that fn.
    pub fn record_closure_parent(
        &mut self,
        sub_closure: hir::ItemLocalId,
        sup_closure: hir::ItemLocalId,
    ) {
        debug!(
            "record_closure_parent(sub_closure={:?}, sup_closure={:?})",
            sub_closure, sup_closure
        );
        assert!(sub_closure != sup_closure);
        let previous = self.closure_tree.insert(sub_closure, sup_closure);
        assert!(previous.is_none());
    }

    pub fn record_var_scope(&mut self, var: hir::ItemLocalId, lifetime: Scope) {
        debug!("record_var_scope(sub={:?}, sup={:?})", var, lifetime);
        assert!(var != lifetime.item_local_id());
        self.var_map.insert(var, lifetime);
    }

    pub fn record_rvalue_scope(&mut self, var: hir::ItemLocalId, lifetime: Option<Scope>) {
        debug!("record_rvalue_scope(sub={:?}, sup={:?})", var, lifetime);
        if let Some(lifetime) = lifetime {
            assert!(var != lifetime.item_local_id());
        }
        self.rvalue_scopes.insert(var, lifetime);
    }

    /// Returns the narrowest scope that encloses `id`, if any.
    pub fn opt_encl_scope(&self, id: Scope) -> Option<Scope> {
        self.parent_map.get(&id).cloned().map(|(p, _)| p)
    }

    /// Returns the narrowest scope that encloses `id`, if any.
    #[allow(dead_code)] // used in cfg
    pub fn encl_scope(&self, id: Scope) -> Scope {
        self.opt_encl_scope(id).unwrap()
    }

    /// Returns the lifetime of the local variable `var_id`
    pub fn var_scope(&self, var_id: hir::ItemLocalId) -> Scope {
        self.var_map
            .get(&var_id)
            .cloned()
            .unwrap_or_else(|| bug!("no enclosing scope for id {:?}", var_id))
    }

    /// Returns the scope when the temp created by `expr_id` will be cleaned up.
    pub fn temporary_scope(&self, expr_id: hir::ItemLocalId) -> Option<Scope> {
        // Check for a designated rvalue scope.
        if let Some(&s) = self.rvalue_scopes.get(&expr_id) {
            debug!("temporary_scope({:?}) = {:?} [custom]", expr_id, s);
            return s;
        }

        // Otherwise, locate the innermost terminating scope
        // if there's one. Static items, for instance, won't
        // have an enclosing scope, hence no scope will be
        // returned.
        let mut id = Scope { id: expr_id, data: ScopeData::Node };

        while let Some(&(p, _)) = self.parent_map.get(&id) {
            match p.data {
                ScopeData::Destruction => {
                    debug!("temporary_scope({:?}) = {:?} [enclosing]", expr_id, id);
                    return Some(id);
                }
                _ => id = p,
            }
        }

        debug!("temporary_scope({:?}) = None", expr_id);
        return None;
    }

    /// Returns the lifetime of the variable `id`.
    pub fn var_region(&self, id: hir::ItemLocalId) -> ty::RegionKind {
        let scope = ty::ReScope(self.var_scope(id));
        debug!("var_region({:?}) = {:?}", id, scope);
        scope
    }

    pub fn scopes_intersect(&self, scope1: Scope, scope2: Scope) -> bool {
        self.is_subscope_of(scope1, scope2) || self.is_subscope_of(scope2, scope1)
    }

    /// Returns `true` if `subscope` is equal to or is lexically nested inside `superscope`, and
    /// `false` otherwise.
    pub fn is_subscope_of(&self, subscope: Scope, superscope: Scope) -> bool {
        let mut s = subscope;
        debug!("is_subscope_of({:?}, {:?})", subscope, superscope);
        while superscope != s {
            match self.opt_encl_scope(s) {
                None => {
                    debug!("is_subscope_of({:?}, {:?}, s={:?})=false", subscope, superscope, s);
                    return false;
                }
                Some(scope) => s = scope,
            }
        }

        debug!("is_subscope_of({:?}, {:?})=true", subscope, superscope);

        return true;
    }

    /// Returns the ID of the innermost containing body.
    pub fn containing_body(&self, mut scope: Scope) -> Option<hir::ItemLocalId> {
        loop {
            if let ScopeData::CallSite = scope.data {
                return Some(scope.item_local_id());
            }

            scope = self.opt_encl_scope(scope)?;
        }
    }

    /// Finds the nearest common ancestor of two scopes. That is, finds the
    /// smallest scope which is greater than or equal to both `scope_a` and
    /// `scope_b`.
    pub fn nearest_common_ancestor(&self, scope_a: Scope, scope_b: Scope) -> Scope {
        if scope_a == scope_b {
            return scope_a;
        }

        let mut a = scope_a;
        let mut b = scope_b;

        // Get the depth of each scope's parent. If either scope has no parent,
        // it must be the root, which means we can stop immediately because the
        // root must be the nearest common ancestor. (In practice, this is
        // moderately common.)
        let (parent_a, parent_a_depth) = match self.parent_map.get(&a) {
            Some(pd) => *pd,
            None => return a,
        };
        let (parent_b, parent_b_depth) = match self.parent_map.get(&b) {
            Some(pd) => *pd,
            None => return b,
        };

        if parent_a_depth > parent_b_depth {
            // `a` is lower than `b`. Move `a` up until it's at the same depth
            // as `b`. The first move up is trivial because we already found
            // `parent_a` above; the loop does the remaining N-1 moves.
            a = parent_a;
            for _ in 0..(parent_a_depth - parent_b_depth - 1) {
                a = self.parent_map.get(&a).unwrap().0;
            }
        } else if parent_b_depth > parent_a_depth {
            // `b` is lower than `a`.
            b = parent_b;
            for _ in 0..(parent_b_depth - parent_a_depth - 1) {
                b = self.parent_map.get(&b).unwrap().0;
            }
        } else {
            // Both scopes are at the same depth, and we know they're not equal
            // because that case was tested for at the top of this function. So
            // we can trivially move them both up one level now.
            assert!(parent_a_depth != 0);
            a = parent_a;
            b = parent_b;
        }

        // Now both scopes are at the same level. We move upwards in lockstep
        // until they match. In practice, this loop is almost always executed
        // zero times because `a` is almost always a direct ancestor of `b` or
        // vice versa.
        while a != b {
            a = self.parent_map.get(&a).unwrap().0;
            b = self.parent_map.get(&b).unwrap().0;
        }

        a
    }

    /// Assuming that the provided region was defined within this `ScopeTree`,
    /// returns the outermost `Scope` that the region outlives.
    pub fn early_free_scope(&self, tcx: TyCtxt<'tcx>, br: &ty::EarlyBoundRegion) -> Scope {
        let param_owner = tcx.parent(br.def_id).unwrap();

        let param_owner_id = tcx.hir().as_local_hir_id(param_owner).unwrap();
        let scope = tcx
            .hir()
            .maybe_body_owned_by(param_owner_id)
            .map(|body_id| tcx.hir().body(body_id).value.hir_id.local_id)
            .unwrap_or_else(|| {
                // The lifetime was defined on node that doesn't own a body,
                // which in practice can only mean a trait or an impl, that
                // is the parent of a method, and that is enforced below.
                if Some(param_owner_id) != self.root_parent {
                    tcx.sess.delay_span_bug(
                        DUMMY_SP,
                        &format!(
                            "free_scope: {:?} not recognized by the \
                              region scope tree for {:?} / {:?}",
                            param_owner,
                            self.root_parent.map(|id| tcx.hir().local_def_id(id)),
                            self.root_body.map(|hir_id| DefId::local(hir_id.owner))
                        ),
                    );
                }

                // The trait/impl lifetime is in scope for the method's body.
                self.root_body.unwrap().local_id
            });

        Scope { id: scope, data: ScopeData::CallSite }
    }

    /// Assuming that the provided region was defined within this `ScopeTree`,
    /// returns the outermost `Scope` that the region outlives.
    pub fn free_scope(&self, tcx: TyCtxt<'tcx>, fr: &ty::FreeRegion) -> Scope {
        let param_owner = match fr.bound_region {
            ty::BoundRegion::BrNamed(def_id, _) => tcx.parent(def_id).unwrap(),
            _ => fr.scope,
        };

        // Ensure that the named late-bound lifetimes were defined
        // on the same function that they ended up being freed in.
        assert_eq!(param_owner, fr.scope);

        let param_owner_id = tcx.hir().as_local_hir_id(param_owner).unwrap();
        let body_id = tcx.hir().body_owned_by(param_owner_id);
        Scope { id: tcx.hir().body(body_id).value.hir_id.local_id, data: ScopeData::CallSite }
    }

    /// Checks whether the given scope contains a `yield`. If so,
    /// returns `Some((span, expr_count))` with the span of a yield we found and
    /// the number of expressions and patterns appearing before the `yield` in the body + 1.
    /// If there a are multiple yields in a scope, the one with the highest number is returned.
    pub fn yield_in_scope(&self, scope: Scope) -> Option<YieldData> {
        self.yield_in_scope.get(&scope).cloned()
    }

    /// Gives the number of expressions visited in a body.
    /// Used to sanity check visit_expr call count when
    /// calculating generator interiors.
    pub fn body_expr_count(&self, body_id: hir::BodyId) -> Option<usize> {
        self.body_expr_count.get(&body_id).copied()
    }
}

impl<'a> HashStable<StableHashingContext<'a>> for ScopeTree {
    fn hash_stable(&self, hcx: &mut StableHashingContext<'a>, hasher: &mut StableHasher) {
        let ScopeTree {
            root_body,
            root_parent,
            ref body_expr_count,
            ref parent_map,
            ref var_map,
            ref destruction_scopes,
            ref rvalue_scopes,
            ref closure_tree,
            ref yield_in_scope,
        } = *self;

        hcx.with_node_id_hashing_mode(NodeIdHashingMode::HashDefPath, |hcx| {
            root_body.hash_stable(hcx, hasher);
            root_parent.hash_stable(hcx, hasher);
        });

        body_expr_count.hash_stable(hcx, hasher);
        parent_map.hash_stable(hcx, hasher);
        var_map.hash_stable(hcx, hasher);
        destruction_scopes.hash_stable(hcx, hasher);
        rvalue_scopes.hash_stable(hcx, hasher);
        closure_tree.hash_stable(hcx, hasher);
        yield_in_scope.hash_stable(hcx, hasher);
    }
}
