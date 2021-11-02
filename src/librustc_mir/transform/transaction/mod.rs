/// Copyright 2021, MASSACHUSETTS INSTITUTE OF TECHNOLOGY
/// Subject to FAR 52.227-11 – Patent Rights – Ownership by the Contractor (May 2014)
/// SPDX-License-Identifier: MIT
//! A pass that identifies shared objects (TxCells) in transactions.
pub use self::conflict_analysis::ConflictAnalysis;
pub use self::use_def_analysis::UseDefVisitor;
use crate::util::patch::MirPatch;
use rustc::hir::def_id::DefId;
use rustc::hir::{Expr, ExprKind, Item, ItemKind, Node};
use rustc::mir::{
    BasicBlock, Body, Constant, Local, Operand, Place, TerminatorKind, Transaction, UniqueId,
};
use rustc::ty::{Const, FnDef, TyCtxt};
use rustc_data_structures::fx::FxHashMap;
use std::convert::TryInto;

pub mod conflict_analysis;
pub mod transaction_map;
pub mod use_def_analysis;

pub fn local_from_dest(destination: &Option<(Place<'tcx>, BasicBlock)>) -> Option<Local> {
    match *destination {
        Some((ref place, _)) => place.local_or_deref_local(),
        _ => None,
    }
}

fn transaction_call(tcx: TyCtxt<'tcx>, is_lock: bool, is_write: bool) -> DefId {
    // We only run when the transaction optimization level is nonzero.
    match tcx.sess.opts.debugging_opts.transaction_level {
        0 => panic!("transaction optimizations are not supported"),
        1 => match is_lock {
            true => tcx.lang_items().transaction_lock().expect("transaction_lock not defined"),
            false => tcx.lang_items().transaction_unlock().expect("transaction_unlock not defined"),
        },
        2 => {
            let lang_items = tcx.lang_items();
            match (is_lock, is_write) {
                (true, true) => {
                    lang_items.transaction_write_lock().expect("transaction_write_lock not defined")
                }
                (true, false) => {
                    lang_items.transaction_read_lock().expect("transaction_read_lock not defined")
                }
                (false, true) => lang_items
                    .transaction_write_unlock()
                    .expect("transaction_write_unlock not defined"),
                (false, false) => lang_items
                    .transaction_read_unlock()
                    .expect("transaction_read_unlock not defined"),
            }
        }
        _ => panic!("unknown transaction optimization level"),
    }
}

fn patch_call(
    body: &Body<'tcx>,
    fn_id: &UniqueId,
    tcx: TyCtxt<'tcx>,
    i: usize,
    is_lock: bool,
    is_write: bool,
) -> TerminatorKind<'tcx> {
    let mut new_term_kind = body[fn_id.location.block].terminator().clone().kind;

    if let TerminatorKind::Call { ref mut func, ref mut args, .. } = new_term_kind {
        let new_def_id = transaction_call(tcx, is_lock, is_write);
        if let Operand::Constant(ref constant) = func {
            if let FnDef(old_def_id, fn_substs) = constant.literal.ty.kind {
                if old_def_id != new_def_id {
                    let new_ty = tcx.mk_ty(FnDef(new_def_id, fn_substs));
                    let new_func = Operand::Constant(box Constant {
                        span: constant.span,
                        user_ty: None,
                        literal: tcx.mk_const(*Const::zero_sized(tcx, new_ty)),
                    });
                    *func = new_func;
                }
            }
        }
        assert_eq!(args.len(), 1);
        if let Operand::Constant(ref constant) = args[0] {
            let new_arg = Operand::Constant(box Constant {
                span: constant.span,
                user_ty: None,
                literal: Const::from_usize(tcx, i.try_into().unwrap()),
            });
            *args = vec![new_arg];
        }
    }
    debug!("[STM] new terminator is {:?}", new_term_kind);
    new_term_kind
}

pub fn make_patches(def_id: DefId, tcx: TyCtxt<'tcx>) -> FxHashMap<DefId, MirPatch<'tcx>> {
    let mut patches: FxHashMap<DefId, MirPatch<'_>> = Default::default();

    if let Some(hir_id) = tcx.hir().as_local_hir_id(def_id) {
        match tcx.hir().find(hir_id) {
            Some(Node::Item(&Item { kind: ItemKind::Fn(..), .. }))
            | Some(Node::Expr(&Expr { kind: ExprKind::Closure(..), .. })) => {
                debug!("[STM] function or closure!! {:?} get patches.", def_id);
                let conflict_sets = tcx.conflict_analysis(def_id.krate);

                for (i, conflict_set) in conflict_sets.iter().enumerate() {
                    debug!("[STM] conflict set {}: {} transactions", i, conflict_set.len());
                    for Transaction { lock, unlock, is_write } in conflict_set {
                        let (body_ref, _) = tcx.mir_validated(lock.def_id);
                        let body = &body_ref.borrow();

                        let patch = patches.entry(lock.def_id).or_insert(MirPatch::new(body));

                        let new_lock = patch_call(body, lock, tcx, i, true, *is_write);
                        let new_unlock = patch_call(body, unlock, tcx, i, false, *is_write);

                        patch.patch_terminator(lock.location.block, new_lock);
                        patch.patch_terminator(unlock.location.block, new_unlock);
                        debug!("[STM] added patches to map");
                    }
                }
            }
            Some(other) => debug!("[STM] other defkind {:?}; no patches {:?}", other, def_id),
            None => debug!("[STM] defkind None; no patches {:?}", def_id),
        }
    }

    patches
}
