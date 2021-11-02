/// Copyright 2021, MASSACHUSETTS INSTITUTE OF TECHNOLOGY
/// Subject to FAR 52.227-11 – Patent Rights – Ownership by the Contractor (May 2014)
/// SPDX-License-Identifier: MIT
use crate::transform::transaction::local_from_dest;
use crate::transform::transaction::transaction_map::TransactionMap;
use crate::util::def_use::{DefUseAnalysis, Use};
use rustc::hir::def_id::{DefId, LOCAL_CRATE};
use rustc::mir::visit::{PlaceContext, Visitor};
use rustc::mir::*;
use rustc::ty::subst::GenericArgKind;
use rustc::ty::{Closure, FnDef, TyCtxt};
use rustc_data_structures::fx::{FxHashMap, FxHashSet};

enum UseKind<'tcx> {
    /// Used in a function with the given DefId as argument # usize.
    Function(Local, DefId, usize),
    /// Used by another Local.
    Local(Local),
    /// Used in a closure with the given arguments.
    ClosureArg(Local, Vec<Operand<'tcx>>),
    /// Used in a final read.
    Read(Local),
    /// Used in a final write.
    Write(Local),
}

/// Find all uses of TxCells/TxPtrs and associate them with a set of unique
/// TxCell/TxPtr allocations.
pub struct UseDefVisitor<'a, 'tcx> {
    analysis: DefUseAnalysis,
    /// The ID of the relevant argument to trace, if relevant
    arg_id: Option<UniqueId>,
    /// The index of the relevant argument to trace, if relevant
    arg_index: Option<usize>,
    body: &'a Body<'tcx>,
    /// Mapping from a transaction ID to the set of shared objects it uses.
    pub allocation_set: FxHashMap<(UniqueId, UniqueId), FxHashSet<TransactionUse>>,
    /// The current allocation whose uses we are following.
    current_allocation: Option<UniqueId>,
    def_id: DefId,
    // Map from a local to all the places it's used.
    pub edges: FxHashMap<UniqueId, FxHashSet<UniqueId>>,
    // Whether the transaction use is a write or read
    is_write: FxHashMap<UniqueId, bool>,
    tcx: TyCtxt<'tcx>,
    /// Map from a terminator ID to the ID of the transaction in which it's contained.
    pub transaction_map: TransactionMap<'a, 'tcx>,
    vertices: FxHashSet<UniqueId>,
}

impl<'tcx> Visitor<'tcx> for UseDefVisitor<'_, 'tcx> {
    /// Visit every terminator in the body. We only need to visit terminators because
    /// function calls are always terminators.
    fn visit_terminator(&mut self, term: &Terminator<'tcx>, location: Location) {
        if let TerminatorKind::Call { func, destination, .. } = &term.kind {
            // TODO: generalize these cases
            let func_name = format!("{:?}", func);
            if !(Self::is_new(&func_name)
                || Self::is_vec(&func_name)
                || Self::is_tree(&func_name)
                || Self::is_arc_vec(&func_name))
            {
                return;
            }
            debug!("[STM] new func {:?}!", func);
            let func_local = local_from_dest(destination).unwrap();
            let func_id = self.unique_id(&func_local, &location, None);
            self.current_allocation = Some(func_id);
            self.vertices.insert(func_id.clone());
            self.trace(func_id);
            self.current_allocation = None;
        }
    }

    fn visit_local(&mut self, &local: &Local, _context: PlaceContext, location: Location) {
        if self.body.local_kind(local) != LocalKind::Arg || self.arg_id.is_none() {
            return;
        }
        if self.arg_index.unwrap() != local.index() - 1 {
            return;
        }
        debug!("[STM] local {:?} is our argument", local);
        let old_arg_id = self.arg_id.unwrap();
        let new_arg_id = match old_arg_id.field {
            Some(field) => {
                debug!("[STM] old arg has field, so new arg needs to go to a field.");
                self.unique_id(&local, &location, Some(field))
            }
            None => {
                debug!("[STM] old arg did not have a field. this is a regular fn call.");
                self.unique_id(&local, &location, None)
            }
        };
        self.connect(old_arg_id, new_arg_id);
        debug!("[STM] new edge thru closure or function call {:?} -> {:?}", old_arg_id, new_arg_id);
        debug!("[STM] recursing into local argument {:?}", new_arg_id);
        self.vertices.insert(new_arg_id.clone());
        self.trace(new_arg_id);
    }
}

impl<'a, 'tcx> UseDefVisitor<'_, 'tcx> {
    /// Create a new UseDefVisitor based on an existing graph.
    fn new_child(
        arg_id: Option<UniqueId>,
        arg_index: Option<usize>,
        body: &'a Body<'tcx>,
        def_id: DefId,
        transaction_map: TransactionMap<'a, 'tcx>,
        parent: &Self,
    ) -> UseDefVisitor<'a, 'tcx> {
        let mut analysis = DefUseAnalysis::new(body);
        analysis.analyze(body);

        UseDefVisitor {
            analysis,
            arg_id,
            arg_index,
            body,
            def_id,
            allocation_set: parent.allocation_set.clone(),
            current_allocation: parent.current_allocation.clone(),
            edges: parent.edges.clone(),
            is_write: parent.is_write.clone(),
            tcx: parent.tcx,
            transaction_map,
            vertices: parent.vertices.clone(),
        }
    }

    /// Create a new UseDefVisitor.
    pub fn new(body: &'a Body<'tcx>, def_id: DefId, tcx: TyCtxt<'tcx>) -> UseDefVisitor<'a, 'tcx> {
        let transaction_map = TransactionMap::new(def_id, body, tcx);
        let mut analysis = DefUseAnalysis::new(body);
        analysis.analyze(body);
        UseDefVisitor {
            analysis,
            arg_id: None,
            arg_index: None,
            body,
            allocation_set: FxHashMap::default(),
            current_allocation: None,
            def_id,
            edges: FxHashMap::default(),
            is_write: FxHashMap::default(),
            tcx,
            transaction_map,
            vertices: FxHashSet::default(),
        }
    }

    pub fn perform(&mut self) -> FxHashMap<(UniqueId, UniqueId), FxHashSet<TransactionUse>> {
        self.transaction_map.perform();
        for (term_id, tx_ids) in &self.transaction_map.terminator_to_tx {
            debug!("[STM] terminator {:?}: tx {:?}", term_id, tx_ids);
        }
        self.visit_body(self.body);
        self.allocation_set.clone()
    }

    /// Return a globally unique ID for a Local.
    fn unique_id(&self, local: &Local, location: &Location, field: Option<usize>) -> UniqueId {
        UniqueId { def_id: self.def_id, local: local.clone(), location: location.clone(), field }
    }

    /// Trace and find all the uses of `use_id`.
    fn trace(&'a mut self, use_id: UniqueId) {
        debug!("[STM] tracing {:?}", use_id.local);
        let info = self.analysis.local_info(use_id.local).clone();

        let uses = info.defs_and_uses.iter().filter(|yuse| yuse.context.is_nonmutating_use());

        for Use { location, .. } in uses {
            debug!("[STM] considering use @ {:?}", location);
            let use_kind = Self::location_to_use_kind(location, &use_id, self.body);
            if use_kind.is_none() {
                continue;
            }
            match use_kind.unwrap() {
                UseKind::Local(new_use_local) => {
                    let new_use_id = self.unique_id(&new_use_local, location, None);
                    self.connect(use_id, new_use_id);
                    debug!("[STM] new edge {:?} -> {:?}", use_id.local, new_use_id.local);
                    if !self.vertices.contains(&new_use_id) {
                        self.vertices.insert(new_use_id.clone());
                        self.trace(new_use_id);
                        continue;
                    }
                    debug!(
                        "[STM] already visited {:?}, so done. uses from here on come from allocation {:?}",
                        new_use_local, self.current_allocation
                    );
                    let use_set = self.use_set(self.edges.get(&new_use_id).unwrap());
                    debug!("[STM] this edge goes down to use set {:?}", use_set);
                    for transaction_use in use_set {
                        self.map_allocation(&transaction_use.shared_object);
                    }
                }
                UseKind::ClosureArg(new_use_local, operands) => {
                    for (i, operand) in operands.iter().enumerate() {
                        if let Some(op_local) = Self::get_local(operand) {
                            if op_local != use_id.local {
                                continue;
                            }
                            debug!(
                                "[STM] closure arg: we care about the {}th field {:?}",
                                i, operand
                            );
                            let new_use_id = self.unique_id(&new_use_local, location, Some(i));
                            self.connect(use_id, new_use_id);
                            debug!("[STM] new edge {:?} -> {:?}", use_id, new_use_id);
                            if !self.vertices.contains(&new_use_id) {
                                self.vertices.insert(new_use_id.clone());
                                self.trace(new_use_id);
                                continue;
                            }
                            debug!(
                                "[STM] already visited {:?}, so done. uses from here on come from allocation {:?}",
                                new_use_local, self.current_allocation
                            );
                            let use_set = self.use_set(self.edges.get(&new_use_id).unwrap());
                            debug!("[STM] this edge goes down to use set {:?}", use_set);
                            for transaction_use in use_set {
                                self.map_allocation(&transaction_use.shared_object);
                            }
                        } else if let Some(use_field) = use_id.field {
                            warn!("[STM] couldn't get local from {:?}", operand);
                            if let Operand::Move(ref place) = operand {
                                if let PlaceBase::Local(op_local) = place.base {
                                    debug!("[STM] got local {:?}", op_local);
                                    if op_local != use_id.local {
                                        debug!("[STM] locals not same, continue...");
                                        continue;
                                    }
                                    for elem in place.projection {
                                        if let ProjectionElem::Field(field, _ty) = elem {
                                            if field.index() == use_field {
                                                debug!(
                                                    "[STM] locals match!! keep tracing yay?!?!?, {:?}",
                                                    place
                                                );
                                                debug!(
                                                    "[STM] closure arg: we care about the {}th field {:?}",
                                                    i, operand
                                                );
                                                let new_use_id = self.unique_id(
                                                    &new_use_local,
                                                    location,
                                                    Some(i),
                                                );
                                                self.connect(use_id, new_use_id);
                                                debug!(
                                                    "[STM] new edge {:?} -> {:?}",
                                                    use_id, new_use_id
                                                );
                                                if !self.vertices.contains(&new_use_id) {
                                                    self.vertices.insert(new_use_id.clone());
                                                    self.trace(new_use_id);
                                                    continue;
                                                }
                                                debug!(
                                                    "[STM] already visited {:?}, so done. uses from here on come from allocation {:?}",
                                                    new_use_local, self.current_allocation
                                                );
                                                let use_set = self
                                                    .use_set(self.edges.get(&new_use_id).unwrap());
                                                debug!(
                                                    "[STM] this edge goes down to use set {:?}",
                                                    use_set
                                                );
                                                for transaction_use in use_set {
                                                    self.map_allocation(
                                                        &transaction_use.shared_object,
                                                    );
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                UseKind::Read(borrow_local) => {
                    let borrow_id = self.unique_id(&borrow_local, location, None);
                    self.is_write.insert(borrow_id, false);
                    self.map_allocation(&borrow_id);
                    self.connect(use_id, borrow_id);
                    debug!(
                        "[STM] new edge from borrow {:?} -> {:?}",
                        use_id.local, borrow_id.local
                    );
                    if self.vertices.contains(&borrow_id) {
                        debug!("[STM] already visited {:?}, so done.", borrow_local);
                        continue;
                    }
                    self.vertices.insert(borrow_id.clone());
                    debug!("[STM] READ, so we're done.");
                }
                UseKind::Write(borrow_local) => {
                    let borrow_id = self.unique_id(&borrow_local, location, None);
                    self.is_write.insert(borrow_id, true);
                    self.map_allocation(&borrow_id);
                    self.connect(use_id, borrow_id);
                    debug!(
                        "[STM] new edge from borrow {:?} -> {:?}",
                        use_id.local, borrow_id.local
                    );
                    if self.vertices.contains(&borrow_id) {
                        debug!("[STM] already visited {:?}, so done.", borrow_local);
                        continue;
                    }
                    self.vertices.insert(borrow_id.clone());
                    debug!("[STM] WRITE, so we're done.");
                }
                UseKind::Function(fn_local, fn_def_id, arg_index) => {
                    debug!(
                        "[STM] considering {:?} -> fn or closure {:?} w/ relevant index {:?}",
                        use_id, fn_def_id, arg_index
                    );
                    if fn_def_id.krate != LOCAL_CRATE {
                        warn!("[STM] non-local def ID {:?}", fn_def_id);
                        return;
                    }
                    let (body_ref, _) = self.tcx.mir_validated(fn_def_id);
                    let fn_body = &body_ref.borrow();

                    debug!("[STM] Relevant arg #{:?} has ID {:?}", arg_index, use_id);

                    let fn_id = self.unique_id(&fn_local, location, None);
                    let tx_ids = self.transaction_map.terminator_to_tx.get(&fn_id);
                    debug!("[STM] function call is inside transaction {:?}", tx_ids);

                    let fn_arg_index = match use_id.field {
                        Some(_) => {
                            debug!("[STM] function is closure, so arg must be 0");
                            Some(0)
                        }
                        None => {
                            debug!("[STM] function is not closure, so arg must be {:?}", arg_index);
                            Some(arg_index)
                        }
                    };

                    let fn_tx_map = TransactionMap::new_child(
                        fn_def_id,
                        fn_body,
                        tx_ids.copied(),
                        self.tcx,
                        self.transaction_map.terminator_to_tx.clone(),
                    );

                    let mut fn_visitor = UseDefVisitor::new_child(
                        Some(use_id),
                        fn_arg_index,
                        fn_body,
                        fn_def_id,
                        fn_tx_map,
                        &self,
                    );

                    fn_visitor.perform();

                    for (terminator_id, tx_ids) in fn_visitor.transaction_map.terminator_to_tx {
                        self.transaction_map.terminator_to_tx.insert(terminator_id, tx_ids);
                    }
                    for (local_id, use_ids) in fn_visitor.edges {
                        self.edges.entry(local_id).or_insert(FxHashSet::default()).extend(use_ids);
                    }
                    self.vertices.extend(fn_visitor.vertices);
                    for (tx_ids, alloc_ids) in fn_visitor.allocation_set {
                        self.allocation_set
                            .entry(tx_ids)
                            .or_insert(FxHashSet::default())
                            .extend(alloc_ids);
                    }
                    for (borrow_id, is_write) in fn_visitor.is_write {
                        self.is_write.insert(borrow_id, is_write);
                    }
                }
            }
        }
    }

    /// Make an edge from src to dst.
    fn connect(&mut self, src: UniqueId, dest: UniqueId) {
        self.edges.entry(src.clone()).or_insert(FxHashSet::default()).insert(dest.clone());
    }

    /// Find the transaction a given borrow is inside, then associate that transaction
    /// with the current allocation.
    fn map_allocation(&mut self, borrow_id: &UniqueId) {
        if let Some(tx_ids) = self.transaction_map.terminator_to_tx.get(&borrow_id) {
            let allocation = self.current_allocation.unwrap();
            let is_write = self.is_write.get(borrow_id).unwrap();
            self.allocation_set
                .entry(tx_ids.clone())
                .or_insert(FxHashSet::default())
                .insert(TransactionUse { shared_object: allocation, is_write: *is_write });
            debug!(
                "[STM] borrow {:?} inside tx {:?} comes from allocation {:?}",
                borrow_id, tx_ids, allocation
            );
        } else {
            warn!("[STM] borrow {:?} is not inside a transaction!", borrow_id);
        }
    }

    /// Follow these uses through the edges map to their terminal leaves (uses).
    fn use_set(&self, edges: &FxHashSet<UniqueId>) -> FxHashSet<TransactionUse> {
        let mut allocations: FxHashSet<TransactionUse> = FxHashSet::default();
        for edge in edges {
            if let Some(new_edges) = self.edges.get(edge) {
                debug!("[STM] -- intermediate edge {:?}", edge);
                allocations.extend(self.use_set(new_edges));
            } else {
                debug!("[STM] terminal use {:?}", edge);
                let is_write = self.is_write.get(edge).unwrap();
                allocations
                    .insert(TransactionUse { shared_object: edge.clone(), is_write: *is_write });
            }
        }
        allocations
    }

    /// Return the UseKind associated with a Location, if any.
    fn location_to_use_kind(
        location: &Location,
        use_id: &UniqueId,
        body: &'a Body<'tcx>,
    ) -> Option<UseKind<'tcx>> {
        let maybe_bb_data = body.basic_blocks().get(location.block);
        if maybe_bb_data.is_none() {
            warn!("[STM] basic blocks do not contain block {:?}", location.block);
            return None;
        }
        let bb_data = maybe_bb_data.unwrap();
        let stmts = &bb_data.statements;

        let index = location.statement_index;
        let length = stmts.len();

        if index > length {
            warn!("[STM] basic block of len {:?} does not contain stmt @ {:?}", length, index);
            return None;
        }
        if index < length {
            let stmt = stmts[index].clone();
            if let StatementKind::Assign(box (ref place, ref rvalue)) = stmt.kind {
                if let Some(local) = place.local_or_deref_local() {
                    // Need to check if this local is the same field.
                    if let Some(use_field) = use_id.field {
                        debug!(
                            "[STM] use has field {:?}, so we need to match the field",
                            use_field
                        );
                        return match rvalue {
                            Rvalue::Use(Operand::Move(ref place)) => {
                                for elem in place.projection {
                                    if let ProjectionElem::Field(field, _ty) = elem {
                                        if field.index() == use_field {
                                            return Some(UseKind::Local(local));
                                        }
                                    }
                                }
                                None
                            }
                            Rvalue::Ref(region, borrow_kind, place) => {
                                info!(
                                    "ref in region {:?} of kind {:?} in place {:?}",
                                    region, borrow_kind, place
                                );
                                for elem in place.projection {
                                    if let ProjectionElem::Field(field, _ty) = elem {
                                        if field.index() == use_field {
                                            return Some(UseKind::Local(local));
                                        }
                                    }
                                }
                                None
                            }
                            Rvalue::Aggregate(box AggregateKind::Closure(..), ops) => {
                                debug!("[STM] aggregate");
                                Some(UseKind::ClosureArg(local, ops.clone()))
                            }
                            _ => {
                                warn!("[STM] unknown rvalue {:?}", rvalue);
                                None
                            }
                        };
                    }
                    if let Rvalue::Aggregate(box AggregateKind::Closure(..), ops) = rvalue {
                        debug!("[STM] statement is a closure aggregate w/ ops {:?}", ops);
                        return Some(UseKind::ClosureArg(local, ops.clone()));
                    }
                    return Some(UseKind::Local(local));
                }
            }
            warn!("[STM] statement is not an Assign statement!");
            return None;
        }
        // index == length, so must be a terminator
        let term = bb_data.terminator.clone().unwrap();
        if let TerminatorKind::Call { func, args, destination, .. } = &term.kind {
            let func_name = format!("{:?}", func);
            if UseDefVisitor::is_read(&func_name) || UseDefVisitor::is_tree_find(&func_name) {
                let local = local_from_dest(destination).unwrap();
                return Some(UseKind::Read(local));
            } else if UseDefVisitor::is_write(&func_name) || UseDefVisitor::is_tree_add(&func_name)
            {
                let local = local_from_dest(destination).unwrap();
                return Some(UseKind::Write(local));
            // TODO: generalize these cases?
            } else if UseDefVisitor::is_deref(&func_name)
                || UseDefVisitor::is_vec_deref(&func_name)
                //|| UseDefVisitor::is_vec_push(&func_name)
                || UseDefVisitor::is_tree_deref(&func_name)
                || UseDefVisitor::is_arc_new(&func_name)
                || UseDefVisitor::is_clone(&func_name)
                || UseDefVisitor::is_vec_index(&func_name)
                || UseDefVisitor::is_arc_vec_index(&func_name)
            {
                let local = local_from_dest(destination).unwrap();
                return Some(UseKind::Local(local));
            }
            debug!("[STM] other terminator {:?}", func);
            if let Operand::Constant(ref constant) = func {
                let ty_kind = &constant.literal.ty.kind;
                debug!("[STM] constant w/ type kind {:?}", ty_kind);
                if let FnDef(fn_def_id, fn_substs) = ty_kind {
                    debug!("[STM] function def w/ def id {:?} substs {:?}", fn_def_id, fn_substs);
                    for kind in fn_substs.iter() {
                        if let GenericArgKind::Type(ty) = kind.unpack() {
                            if let Closure(closure_def_id, closure_substs) = ty.kind {
                                debug!(
                                    "the ty is a closure w/ def id {:?}, substs {:?}",
                                    closure_def_id, closure_substs
                                );
                                // Closures pack their arguments into a tuple.
                                if let Some(field) = use_id.field {
                                    debug!("[STM] we care about the closure's {:?}th field", field);
                                    let local = local_from_dest(destination).unwrap();
                                    return Some(UseKind::Function(local, closure_def_id, field));
                                } else {
                                    warn!(
                                        "this is a closure, so prev use {:?} should put args into a tuple",
                                        use_id
                                    );
                                }
                            }
                        }
                    }
                    for (i, arg) in args.iter().enumerate() {
                        if let Some(arg_local) = UseDefVisitor::get_local(arg) {
                            if arg_local != use_id.local {
                                continue;
                            }
                            debug!("[STM] we care about the {}th function argument {:?}", i, arg);
                            let local = local_from_dest(destination).unwrap();
                            return Some(UseKind::Function(local, fn_def_id.clone(), i));
                        }
                    }
                }
            }
        }
        warn!("[STM] loc {:?} has no definition in body?", location);
        None
    }

    /// Return the Local associated with an Operand, if it has one.
    /// TODO: just return PlaceBase::Local(local)?
    fn get_local(operand: &Operand<'tcx>) -> Option<Local> {
        match operand {
            Operand::Copy(ref place) => place.local_or_deref_local(),
            Operand::Move(ref place) => place.local_or_deref_local(),
            Operand::Constant(_) => None,
        }
    }

    /// Check if the function is txcell::TxPtr::<.*>::new.
    /// e.g. const txcell::TxPtr::<i32>::new()
    fn is_new(func_name: &str) -> bool {
        func_name.starts_with("const txcell::TxPtr::<") && func_name.ends_with(">::new")
    }

    fn is_arc_vec(func_name: &str) -> bool {
        func_name.starts_with("const std::vec::Vec::<std::sync::Arc<txcell::TxPtr<")
            && func_name.ends_with(">>>::new")
    }

    fn is_vec(func_name: &str) -> bool {
        func_name.starts_with("const std::vec::Vec::<txcell::TxPtr<")
            && func_name.ends_with(">>::new")
    }

    fn is_vec_index(func_name: &str) -> bool {
        // TODO: index on things other than usize?
        func_name.starts_with("const <std::vec::Vec<txcell::TxPtr<")
            && func_name.ends_with(">> as std::ops::Index<usize>>::index")
    }

    fn is_arc_vec_index(func_name: &str) -> bool {
        func_name.starts_with("const <std::vec::Vec<std::sync::Arc<txcell::TxPtr<")
            && func_name.ends_with(">>> as std::ops::Index<usize>>::index")
    }

    fn is_tree(func_name: &str) -> bool {
        func_name.starts_with("const txcell::tree::BinarySearchTree::<")
            && func_name.ends_with(">::new")
    }

    fn is_tree_find(func_name: &str) -> bool {
        func_name.starts_with("const txcell::tree::BinarySearchTree::<")
            && func_name.ends_with(">::find")
    }

    fn is_tree_add(func_name: &str) -> bool {
        func_name.starts_with("const txcell::tree::BinarySearchTree::<")
            && func_name.ends_with(">::add")
    }

    /// Check if the function is a Deref.
    fn is_deref(func_name: &str) -> bool {
        func_name.starts_with("const <std::sync::Arc<txcell::TxPtr<")
            && func_name.ends_with(">> as std::ops::Deref>::deref")
    }

    /// Check if the function is a Deref.
    fn is_vec_deref(func_name: &str) -> bool {
        func_name.starts_with("const <std::sync::Arc<std::vec::Vec<txcell::TxPtr<")
            && func_name.ends_with(">>> as std::ops::Deref>::deref")
    }

    /// Check if the function is a Deref.
    fn is_tree_deref(func_name: &str) -> bool {
        func_name.starts_with("const <std::sync::Arc<txcell::tree::BinarySearchTree<")
            && func_name.ends_with(">> as std::ops::Deref>::deref")
    }

    /// Check if the function is an Arc::new.
    fn is_arc_new(func_name: &str) -> bool {
        func_name.starts_with("const std::sync::Arc::<") && func_name.ends_with(">::new")
    }

    /// Check if the function is an Arc::clone.
    fn is_clone(func_name: &str) -> bool {
        func_name.ends_with("> as std::clone::Clone>::clone")
    }

    /// Check if the function is txcell::TxPtr::<.*>::borrow.
    fn is_read(func_name: &str) -> bool {
        func_name.starts_with("const txcell::TxPtr::<") && func_name.ends_with(">::borrow")
    }

    /// Check if the function is txcell::TxPtr::<.*>::borrow_mut.
    fn is_write(func_name: &str) -> bool {
        func_name.starts_with("const txcell::TxPtr::<") && func_name.ends_with(">::borrow_mut")
    }
}
