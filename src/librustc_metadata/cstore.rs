// The crate store - a central repo for information collected about external
// crates and libraries

use crate::schema;
use rustc::dep_graph::DepNodeIndex;
use rustc::hir::def_id::{CrateNum, DefIndex};
use rustc::hir::map::definitions::DefPathTable;
use rustc::middle::cstore::{CrateSource, DepKind, ExternCrate};
use rustc::mir::interpret::AllocDecodingState;
use rustc_index::vec::IndexVec;
use rustc::util::nodemap::FxHashMap;
use rustc_data_structures::sync::{Lrc, Lock, MetadataRef, Once, AtomicCell};
use rustc_data_structures::svh::Svh;
use syntax::ast;
use syntax::edition::Edition;
use syntax_expand::base::SyntaxExtension;
use syntax_pos;
use proc_macro::bridge::client::ProcMacro;

pub use crate::cstore_impl::{provide, provide_extern};

// A map from external crate numbers (as decoded from some crate file) to
// local crate numbers (as generated during this session). Each external
// crate may refer to types in other external crates, and each has their
// own crate numbers.
crate type CrateNumMap = IndexVec<CrateNum, CrateNum>;

crate struct MetadataBlob(pub MetadataRef);

/// Holds information about a syntax_pos::SourceFile imported from another crate.
/// See `imported_source_files()` for more information.
crate struct ImportedSourceFile {
    /// This SourceFile's byte-offset within the source_map of its original crate
    pub original_start_pos: syntax_pos::BytePos,
    /// The end of this SourceFile within the source_map of its original crate
    pub original_end_pos: syntax_pos::BytePos,
    /// The imported SourceFile's representation within the local source_map
    pub translated_source_file: Lrc<syntax_pos::SourceFile>,
}

crate struct CrateMetadata {
    /// The primary crate data - binary metadata blob.
    crate blob: MetadataBlob,

    // --- Some data pre-decoded from the metadata blob, usually for performance ---

    /// Properties of the whole crate.
    /// NOTE(eddyb) we pass `'static` to a `'tcx` parameter because this
    /// lifetime is only used behind `Lazy`, and therefore acts like an
    /// universal (`for<'tcx>`), that is paired up with whichever `TyCtxt`
    /// is being used to decode those values.
    crate root: schema::CrateRoot<'static>,
    /// For each definition in this crate, we encode a key. When the
    /// crate is loaded, we read all the keys and put them in this
    /// hashmap, which gives the reverse mapping. This allows us to
    /// quickly retrace a `DefPath`, which is needed for incremental
    /// compilation support.
    crate def_path_table: DefPathTable,
    /// Trait impl data.
    /// FIXME: Used only from queries and can use query cache,
    /// so pre-decoding can probably be avoided.
    crate trait_impls: FxHashMap<(u32, DefIndex), schema::Lazy<[DefIndex]>>,
    /// Proc macro descriptions for this crate, if it's a proc macro crate.
    crate raw_proc_macros: Option<&'static [ProcMacro]>,
    /// Source maps for code from the crate.
    crate source_map_import_info: Once<Vec<ImportedSourceFile>>,
    /// Used for decoding interpret::AllocIds in a cached & thread-safe manner.
    crate alloc_decoding_state: AllocDecodingState,
    /// The `DepNodeIndex` of the `DepNode` representing this upstream crate.
    /// It is initialized on the first access in `get_crate_dep_node_index()`.
    /// Do not access the value directly, as it might not have been initialized yet.
    /// The field must always be initialized to `DepNodeIndex::INVALID`.
    crate dep_node_index: AtomicCell<DepNodeIndex>,

    // --- Other significant crate properties ---

    /// ID of this crate, from the current compilation session's point of view.
    crate cnum: CrateNum,
    /// Maps crate IDs as they are were seen from this crate's compilation sessions into
    /// IDs as they are seen from the current compilation session.
    crate cnum_map: CrateNumMap,
    /// Same ID set as `cnum_map` plus maybe some injected crates like panic runtime.
    crate dependencies: Lock<Vec<CrateNum>>,
    /// How to link (or not link) this crate to the currently compiled crate.
    crate dep_kind: Lock<DepKind>,
    /// Filesystem location of this crate.
    crate source: CrateSource,
    /// Whether or not this crate should be consider a private dependency
    /// for purposes of the 'exported_private_dependencies' lint
    crate private_dep: bool,
    /// The hash for the host proc macro. Used to support `-Z dual-proc-macro`.
    crate host_hash: Option<Svh>,

    // --- Data used only for improving diagnostics ---

    /// Information about the `extern crate` item or path that caused this crate to be loaded.
    /// If this is `None`, then the crate was injected (e.g., by the allocator).
    crate extern_crate: Lock<Option<ExternCrate>>,
}

#[derive(Clone)]
pub struct CStore {
    metas: IndexVec<CrateNum, Option<Lrc<CrateMetadata>>>,
}

pub enum LoadedMacro {
    MacroDef(ast::Item, Edition),
    ProcMacro(SyntaxExtension),
}

impl Default for CStore {
    fn default() -> Self {
        CStore {
            // We add an empty entry for LOCAL_CRATE (which maps to zero) in
            // order to make array indices in `metas` match with the
            // corresponding `CrateNum`. This first entry will always remain
            // `None`.
            metas: IndexVec::from_elem_n(None, 1),
        }
    }
}

impl CStore {
    crate fn alloc_new_crate_num(&mut self) -> CrateNum {
        self.metas.push(None);
        CrateNum::new(self.metas.len() - 1)
    }

    crate fn get_crate_data(&self, cnum: CrateNum) -> &CrateMetadata {
        self.metas[cnum].as_ref()
            .unwrap_or_else(|| panic!("Failed to get crate data for {:?}", cnum))
    }

    crate fn set_crate_data(&mut self, cnum: CrateNum, data: CrateMetadata) {
        assert!(self.metas[cnum].is_none(), "Overwriting crate metadata entry");
        self.metas[cnum] = Some(Lrc::new(data));
    }

    crate fn iter_crate_data<I>(&self, mut i: I)
        where I: FnMut(CrateNum, &CrateMetadata)
    {
        for (k, v) in self.metas.iter_enumerated() {
            if let &Some(ref v) = v {
                i(k, v);
            }
        }
    }

    crate fn crate_dependencies_in_rpo(&self, krate: CrateNum) -> Vec<CrateNum> {
        let mut ordering = Vec::new();
        self.push_dependencies_in_postorder(&mut ordering, krate);
        ordering.reverse();
        ordering
    }

    crate fn push_dependencies_in_postorder(&self, ordering: &mut Vec<CrateNum>, krate: CrateNum) {
        if ordering.contains(&krate) {
            return;
        }

        let data = self.get_crate_data(krate);
        for &dep in data.dependencies.borrow().iter() {
            if dep != krate {
                self.push_dependencies_in_postorder(ordering, dep);
            }
        }

        ordering.push(krate);
    }

    crate fn do_postorder_cnums_untracked(&self) -> Vec<CrateNum> {
        let mut ordering = Vec::new();
        for (num, v) in self.metas.iter_enumerated() {
            if let &Some(_) = v {
                self.push_dependencies_in_postorder(&mut ordering, num);
            }
        }
        return ordering
    }
}
