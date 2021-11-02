/// Copyright 2021, MASSACHUSETTS INSTITUTE OF TECHNOLOGY
/// Subject to FAR 52.227-11 – Patent Rights – Ownership by the Contractor (May 2014)
/// SPDX-License-Identifier: MIT
use crate::transform::transaction::local_from_dest;
use rustc::hir::def_id::DefId;
use rustc::mir::visit::Visitor;
use rustc::mir::*;
use rustc::ty::{FnDef, TyCtxt};
use rustc_data_structures::fx::FxHashMap;

/// Mark every terminator within this DefId as part or not part of a transaction.
#[derive(Clone)]
pub struct TransactionMap<'a, 'tcx> {
    body: &'a Body<'tcx>,
    def_id: DefId,
    lock_def_id: Option<DefId>,
    unlock_def_id: Option<DefId>,
    /// Map from a terminator ID to the ID of the lock call of the transaction in which it's contained.
    pub terminator_to_lock: FxHashMap<UniqueId, UniqueId>,
    /// Map from a transaction's lock ID to its unlock ID.
    pub lock_to_unlock: FxHashMap<UniqueId, UniqueId>,
    /// Map from a terminator ID to the lock and unlock ID of the transaction in which it's contained.
    pub terminator_to_tx: FxHashMap<UniqueId, (UniqueId, UniqueId)>,
    transaction_id: Option<UniqueId>,
}

impl<'tcx> Visitor<'tcx> for TransactionMap<'_, 'tcx> {
    /// Visit every terminator in the body. We only need to visit terminators because
    /// function calls are always terminators.
    fn visit_terminator(&mut self, term: &Terminator<'tcx>, location: Location) {
        if let TerminatorKind::Call { func, destination, .. } = &term.kind {
            if let Operand::Constant(ref constant) = func {
                let ty_kind = &constant.literal.ty.kind;
                if let FnDef(fn_def_id, _substs) = ty_kind {
                    if *fn_def_id == self.lock_def_id.unwrap() {
                        let func_local = local_from_dest(destination).unwrap();
                        let func_id = self.unique_id(&func_local, &location);
                        debug!("[STM] LOCK: we are in transaction {:?}", func_id);
                        self.transaction_id = Some(func_id);
                    } else if *fn_def_id == self.unlock_def_id.unwrap() {
                        if let Some(lock_id) = &self.transaction_id {
                            debug!("[STM] UNLOCK: we are no longer in transaction {:?}", lock_id);
                            let func_local = local_from_dest(destination).unwrap();
                            let unlock_id = self.unique_id(&func_local, &location);
                            self.lock_to_unlock.insert(lock_id.clone(), unlock_id);
                            self.transaction_id = None;
                        } else {
                            warn!("[STM] double unlock!");
                        }
                    } else if let Some(tx_id) = &self.transaction_id {
                        if let Some(func_local) = local_from_dest(destination) {
                            let func_id = self.unique_id(&func_local, &location);
                            self.terminator_to_lock.insert(func_id, tx_id.clone());
                        }
                    }
                }
            }
        }
    }
}

impl<'a, 'tcx> TransactionMap<'_, 'tcx> {
    /// Create a new TransactionMap based on an existing one.
    pub fn new_child(
        def_id: DefId,
        body: &'a Body<'tcx>,
        transaction_ids: Option<(UniqueId, UniqueId)>,
        tcx: TyCtxt<'tcx>,
        terminator_to_tx: FxHashMap<UniqueId, (UniqueId, UniqueId)>,
    ) -> TransactionMap<'a, 'tcx> {
        let lock_def_id = tcx.lang_items().transaction_lock();
        let unlock_def_id = tcx.lang_items().transaction_unlock();

        let mut lock_to_unlock = FxHashMap::default();
        let transaction_id = match transaction_ids {
            None => None,
            Some((lock_id, unlock_id)) => {
                lock_to_unlock.insert(lock_id, unlock_id);
                Some(lock_id)
            }
        };

        TransactionMap {
            body,
            def_id,
            lock_def_id,
            unlock_def_id,
            terminator_to_lock: FxHashMap::default(),
            lock_to_unlock,
            terminator_to_tx,
            transaction_id,
        }
    }

    /// Create a new TransactionMap.
    pub fn new(def_id: DefId, body: &'a Body<'tcx>, tcx: TyCtxt<'tcx>) -> TransactionMap<'a, 'tcx> {
        let lock_def_id = tcx.lang_items().transaction_lock();
        let unlock_def_id = tcx.lang_items().transaction_unlock();
        TransactionMap {
            body,
            def_id,
            lock_def_id,
            unlock_def_id,
            terminator_to_lock: FxHashMap::default(),
            lock_to_unlock: FxHashMap::default(),
            terminator_to_tx: FxHashMap::default(),
            transaction_id: None,
        }
    }

    pub fn perform(&mut self) {
        if let (Some(_), Some(_)) = (self.lock_def_id, self.unlock_def_id) {
            for (block, block_data) in traversal::reverse_postorder(self.body) {
                self.visit_basic_block_data(block, block_data);
            }
        }
        for (term, lock) in self.terminator_to_lock.iter() {
            let unlock = self.lock_to_unlock.get(lock).unwrap();
            self.terminator_to_tx.insert(*term, (*lock, *unlock));
        }
    }

    /// Create a globally unique ID for a Local.
    fn unique_id(&self, local: &Local, location: &Location) -> UniqueId {
        UniqueId {
            def_id: self.def_id,
            local: local.clone(),
            location: location.clone(),
            field: None,
        }
    }
}
