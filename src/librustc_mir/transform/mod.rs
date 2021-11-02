//! Modifications from Rust sha id: d1fff4a4b213b3341c1ff994061b7965a5932c70
//! Copyright 2021, MASSACHUSETTS INSTITUTE OF TECHNOLOGY
//! Subject to FAR 52.227-11 – Patent Rights – Ownership by the Contractor (May 2014).
//! SPDX-License-Identifier: MIT
//!
use crate::{build, shim};
use rustc::hir;
use rustc::hir::def_id::{CrateNum, DefId, LOCAL_CRATE};
use rustc::hir::intravisit::{self, NestedVisitorMap, Visitor};
use rustc::mir::{AllocationSet, Body, MirPhase, Promoted, Transaction};
use rustc::ty::query::Providers;
use rustc::ty::steal::Steal;
use rustc::ty::{InstanceDef, TyCtxt};
use rustc::util::nodemap::DefIdSet;
use rustc_index::vec::IndexVec;
use std::borrow::Cow;
use std::iter::FromIterator;
use syntax::ast;
use syntax_pos::Span;
use transaction::{
    conflict_analysis::ConflictAnalysis, make_patches, use_def_analysis::UseDefVisitor,
};

pub mod add_call_guards;
pub mod add_moves_for_packed_drops;
pub mod add_retag;
pub mod check_consts;
pub mod check_unsafety;
pub mod cleanup_post_borrowck;
pub mod const_prop;
pub mod copy_prop;
pub mod deaggregator;
pub mod dump_mir;
pub mod elaborate_drops;
pub mod erase_regions;
pub mod generator;
pub mod inline;
pub mod instcombine;
pub mod no_landing_pads;
pub mod promote_consts;
pub mod qualify_consts;
pub mod qualify_min_const_fn;
pub mod remove_noop_landing_pads;
pub mod rustc_peek;
pub mod simplify;
pub mod simplify_branches;
pub mod transaction;
pub mod uniform_array_move_out;

pub(crate) fn provide(providers: &mut Providers<'_>) {
    self::qualify_consts::provide(providers);
    self::check_unsafety::provide(providers);
    *providers = Providers {
        mir_keys,
        mir_built,
        mir_const,
        mir_validated,
        optimized_mir,
        is_mir_available,
        promoted_mir,
        conflict_analysis,
        get_shared_objects,
        ..*providers
    };
}

fn is_mir_available(tcx: TyCtxt<'_>, def_id: DefId) -> bool {
    tcx.mir_keys(def_id.krate).contains(&def_id)
}

/// Finds the full set of `DefId`s within the current crate that have
/// MIR associated with them.
fn mir_keys(tcx: TyCtxt<'_>, krate: CrateNum) -> &DefIdSet {
    assert_eq!(krate, LOCAL_CRATE);

    let mut set = DefIdSet::default();

    // All body-owners have MIR associated with them.
    set.extend(tcx.body_owners());

    // Additionally, tuple struct/variant constructors have MIR, but
    // they don't have a BodyId, so we need to build them separately.
    struct GatherCtors<'a, 'tcx> {
        tcx: TyCtxt<'tcx>,
        set: &'a mut DefIdSet,
    }
    impl<'a, 'tcx> Visitor<'tcx> for GatherCtors<'a, 'tcx> {
        fn visit_variant_data(
            &mut self,
            v: &'tcx hir::VariantData,
            _: ast::Name,
            _: &'tcx hir::Generics,
            _: hir::HirId,
            _: Span,
        ) {
            if let hir::VariantData::Tuple(_, hir_id) = *v {
                self.set.insert(self.tcx.hir().local_def_id(hir_id));
            }
            intravisit::walk_struct_def(self, v)
        }
        fn nested_visit_map<'b>(&'b mut self) -> NestedVisitorMap<'b, 'tcx> {
            NestedVisitorMap::None
        }
    }
    tcx.hir()
        .krate()
        .visit_all_item_likes(&mut GatherCtors { tcx, set: &mut set }.as_deep_visitor());

    tcx.arena.alloc(set)
}

fn get_shared_objects(tcx: TyCtxt<'_>, def_id: DefId) -> Vec<AllocationSet> {
    let (body, _) = tcx.mir_validated(def_id);

    // Perform use-def analysis to determine allocation set
    let allocation_set = UseDefVisitor::new(&body.borrow(), def_id, tcx).perform();

    let num_transactions = allocation_set.len();

    let mut shared_objects = vec![];

    if num_transactions > 0 {
        info!("[STM] done: printing {} transactions", num_transactions);
        for ((lock_id, unlock_id), allocation_set) in &allocation_set {
            info!("[STM] tx {:?}: {} allocations", lock_id, allocation_set.len());
            shared_objects.push(AllocationSet {
                lock: *lock_id,
                unlock: *unlock_id,
                allocations: Vec::from_iter(allocation_set.clone()),
            });
        }
    }
    shared_objects
}

fn conflict_analysis(tcx: TyCtxt<'_>, crate_num: CrateNum) -> Vec<Vec<Transaction>> {
    info!("[STM] performing CA start");

    let mut all = vec![];
    for def_id in tcx.mir_keys(crate_num) {
        info!("[STM] considering {:?}", def_id);
        if !tcx.is_const_fn(def_id.clone()) {
            let shared_objs = tcx.get_shared_objects(*def_id);
            all.extend(shared_objs);
        }
    }
    info!("[STM] consider all shared objects {:?}", all);

    // Perform conflict analysis on all the shared objects here.
    let ca = ConflictAnalysis::new(all).perform();
    info!("[STM] performing CA done");
    ca.into_iter().map(|hs| Vec::from_iter(hs)).collect()
}

fn mir_built(tcx: TyCtxt<'_>, def_id: DefId) -> &Steal<Body<'_>> {
    let mir = build::mir_build(tcx, def_id);
    //info!("[STM] MIR is {:#?}", mir);
    tcx.alloc_steal_mir(mir)
}

/// Where a specific `mir::Body` comes from.
#[derive(Debug, Copy, Clone)]
pub struct MirSource<'tcx> {
    pub instance: InstanceDef<'tcx>,

    /// If `Some`, this is a promoted rvalue within the parent function.
    pub promoted: Option<Promoted>,
}

impl<'tcx> MirSource<'tcx> {
    pub fn item(def_id: DefId) -> Self {
        MirSource { instance: InstanceDef::Item(def_id), promoted: None }
    }

    #[inline]
    pub fn def_id(&self) -> DefId {
        self.instance.def_id()
    }
}

/// Generates a default name for the pass based on the name of the
/// type `T`.
pub fn default_name<T: ?Sized>() -> Cow<'static, str> {
    let name = ::std::any::type_name::<T>();
    if let Some(tail) = name.rfind(":") { Cow::from(&name[tail + 1..]) } else { Cow::from(name) }
}

/// A streamlined trait that you can implement to create a pass; the
/// pass will be named after the type, and it will consist of a main
/// loop that goes over each available MIR and applies `run_pass`.
pub trait MirPass<'tcx> {
    fn name(&self) -> Cow<'_, str> {
        default_name::<Self>()
    }

    fn run_pass(&self, tcx: TyCtxt<'tcx>, source: MirSource<'tcx>, body: &mut Body<'tcx>);
}

pub fn run_passes(
    tcx: TyCtxt<'tcx>,
    body: &mut Body<'tcx>,
    instance: InstanceDef<'tcx>,
    promoted: Option<Promoted>,
    mir_phase: MirPhase,
    passes: &[&dyn MirPass<'tcx>],
) {
    let phase_index = mir_phase.phase_index();

    if body.phase >= mir_phase {
        return;
    }

    let source = MirSource { instance, promoted };
    let mut index = 0;
    let mut run_pass = |pass: &dyn MirPass<'tcx>| {
        let run_hooks = |body: &_, index, is_after| {
            dump_mir::on_mir_pass(
                tcx,
                &format_args!("{:03}-{:03}", phase_index, index),
                &pass.name(),
                source,
                body,
                is_after,
            );
        };
        run_hooks(body, index, false);
        pass.run_pass(tcx, source, body);
        run_hooks(body, index, true);

        index += 1;
    };

    for pass in passes {
        run_pass(*pass);
    }

    body.phase = mir_phase;
}

fn mir_const(tcx: TyCtxt<'_>, def_id: DefId) -> &Steal<Body<'_>> {
    // Unsafety check uses the raw mir, so make sure it is run
    let _ = tcx.unsafety_check_result(def_id);

    let mut body = tcx.mir_built(def_id).steal();
    run_passes(
        tcx,
        &mut body,
        InstanceDef::Item(def_id),
        None,
        MirPhase::Const,
        &[
            // What we need to do constant evaluation.
            &simplify::SimplifyCfg::new("initial"),
            &rustc_peek::SanityCheck,
            &uniform_array_move_out::UniformArrayMoveOut,
        ],
    );
    tcx.alloc_steal_mir(body)
}

fn mir_validated(
    tcx: TyCtxt<'tcx>,
    def_id: DefId,
) -> (&'tcx Steal<Body<'tcx>>, &'tcx Steal<IndexVec<Promoted, Body<'tcx>>>) {
    let hir_id = tcx.hir().as_local_hir_id(def_id).unwrap();
    if let hir::BodyOwnerKind::Const = tcx.hir().body_owner_kind(hir_id) {
        // Ensure that we compute the `mir_const_qualif` for constants at
        // this point, before we steal the mir-const result.
        let _ = tcx.mir_const_qualif(def_id);
    }

    let mut body = tcx.mir_const(def_id).steal();
    let qualify_and_promote_pass = qualify_consts::QualifyAndPromoteConstants::default();
    run_passes(
        tcx,
        &mut body,
        InstanceDef::Item(def_id),
        None,
        MirPhase::Validated,
        &[
            // What we need to run borrowck etc.
            &qualify_and_promote_pass,
            &simplify::SimplifyCfg::new("qualify-consts"),
        ],
    );
    let promoted = qualify_and_promote_pass.promoted.into_inner();
    (tcx.alloc_steal_mir(body), tcx.alloc_steal_promoted(promoted))
}

fn run_optimization_passes<'tcx>(
    tcx: TyCtxt<'tcx>,
    body: &mut Body<'tcx>,
    def_id: DefId,
    promoted: Option<Promoted>,
) {
    run_passes(
        tcx,
        body,
        InstanceDef::Item(def_id),
        promoted,
        MirPhase::Optimized,
        &[
            // Remove all things only needed by analysis
            &no_landing_pads::NoLandingPads::new(tcx),
            &simplify_branches::SimplifyBranches::new("initial"),
            &remove_noop_landing_pads::RemoveNoopLandingPads,
            &cleanup_post_borrowck::CleanupNonCodegenStatements,
            &simplify::SimplifyCfg::new("early-opt"),
            // These next passes must be executed together
            &add_call_guards::CriticalCallEdges,
            &elaborate_drops::ElaborateDrops,
            &no_landing_pads::NoLandingPads::new(tcx),
            // AddMovesForPackedDrops needs to run after drop
            // elaboration.
            &add_moves_for_packed_drops::AddMovesForPackedDrops,
            // AddRetag needs to run after ElaborateDrops, and it needs
            // an AllCallEdges pass right before it.  Otherwise it should
            // run fairly late, but before optimizations begin.
            &add_call_guards::AllCallEdges,
            &add_retag::AddRetag,
            &simplify::SimplifyCfg::new("elaborate-drops"),
            // No lifetime analysis based on borrowing can be done from here on out.

            // From here on out, regions are gone.
            &erase_regions::EraseRegions,
            // Optimizations begin.
            &uniform_array_move_out::RestoreSubsliceArrayMoveOut::new(tcx),
            &inline::Inline,
            // Lowering generator control-flow and variables
            // has to happen before we do anything else to them.
            &generator::StateTransform,
            &instcombine::InstCombine,
            &const_prop::ConstProp,
            &simplify_branches::SimplifyBranches::new("after-const-prop"),
            &deaggregator::Deaggregator,
            &copy_prop::CopyPropagation,
            &simplify_branches::SimplifyBranches::new("after-copy-prop"),
            &remove_noop_landing_pads::RemoveNoopLandingPads,
            &simplify::SimplifyCfg::new("final"),
            &simplify::SimplifyLocals,
            &add_call_guards::CriticalCallEdges,
            &dump_mir::Marker("PreCodegen"),
        ],
    );
}

fn optimized_mir(tcx: TyCtxt<'_>, def_id: DefId) -> &Body<'_> {
    if tcx.is_constructor(def_id) {
        // There's no reason to run all of the MIR passes on constructors when
        // we can just output the MIR we want directly. This also saves const
        // qualification and borrow checking the trouble of special casing
        // constructors.
        return shim::build_adt_ctor(tcx, def_id);
    }

    // (Mir-)Borrowck uses `mir_validated`, so we have to force it to
    // execute before we can steal.
    tcx.ensure().mir_borrowck(def_id);

    // conflict analysis uses `mir_validated`, so we have to force it to
    // execute before we can steal.
    let mut patches = make_patches(def_id, tcx);

    let (body, _) = tcx.mir_validated(def_id);
    // [STM] this causes a huge performance hit.
    let mut body = (*body.borrow()).clone(); // used to be body.steal()

    if let Some(patch) = patches.remove(&def_id) {
        info!("[STM] applying patch...");
        patch.apply(&mut body);
        info!("[STM] applied patch");
    }

    run_optimization_passes(tcx, &mut body, def_id, None);
    tcx.arena.alloc(body)
}

fn promoted_mir<'tcx>(tcx: TyCtxt<'tcx>, def_id: DefId) -> &'tcx IndexVec<Promoted, Body<'tcx>> {
    if tcx.is_constructor(def_id) {
        return tcx.intern_promoted(IndexVec::new());
    }

    tcx.ensure().mir_borrowck(def_id);
    let (_, promoted) = tcx.mir_validated(def_id);
    let mut promoted = promoted.steal();

    for (p, mut body) in promoted.iter_enumerated_mut() {
        run_optimization_passes(tcx, &mut body, def_id, Some(p));
    }

    tcx.intern_promoted(promoted)
}
