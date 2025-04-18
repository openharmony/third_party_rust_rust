use crate::{NameBinding, NameBindingKind, Resolver, ResolverTree};
use rustc_ast::ast;
use rustc_ast::visit;
use rustc_ast::visit::Visitor;
use rustc_ast::Crate;
use rustc_ast::EnumDef;
use rustc_data_structures::intern::Interned;
use rustc_hir::def_id::LocalDefId;
use rustc_hir::def_id::CRATE_DEF_ID;
use rustc_middle::middle::privacy::{EffectiveVisibilities, EffectiveVisibility};
use rustc_middle::middle::privacy::{IntoDefIdTree, Level};
use rustc_middle::ty::{DefIdTree, Visibility};
use std::mem;

type ImportId<'a> = Interned<'a, NameBinding<'a>>;

#[derive(Clone, Copy)]
enum ParentId<'a> {
    Def(LocalDefId),
    Import(ImportId<'a>),
}

impl ParentId<'_> {
    fn level(self) -> Level {
        match self {
            ParentId::Def(_) => Level::Direct,
            ParentId::Import(_) => Level::Reexported,
        }
    }
}

pub(crate) struct EffectiveVisibilitiesVisitor<'r, 'a, 'tcx> {
    r: &'r mut Resolver<'a, 'tcx>,
    def_effective_visibilities: EffectiveVisibilities,
    /// While walking import chains we need to track effective visibilities per-binding, and def id
    /// keys in `Resolver::effective_visibilities` are not enough for that, because multiple
    /// bindings can correspond to a single def id in imports. So we keep a separate table.
    import_effective_visibilities: EffectiveVisibilities<ImportId<'a>>,
    // It's possible to recalculate this at any point, but it's relatively expensive.
    current_private_vis: Visibility,
    changed: bool,
}

impl Resolver<'_, '_> {
    fn nearest_normal_mod(&mut self, def_id: LocalDefId) -> LocalDefId {
        self.get_nearest_non_block_module(def_id.to_def_id()).nearest_parent_mod().expect_local()
    }

    fn private_vis_import(&mut self, binding: ImportId<'_>) -> Visibility {
        let NameBindingKind::Import { import, .. } = binding.kind else { unreachable!() };
        Visibility::Restricted(
            import
                .id()
                .map(|id| self.nearest_normal_mod(self.local_def_id(id)))
                .unwrap_or(CRATE_DEF_ID),
        )
    }

    fn private_vis_def(&mut self, def_id: LocalDefId) -> Visibility {
        // For mod items `nearest_normal_mod` returns its argument, but we actually need its parent.
        let normal_mod_id = self.nearest_normal_mod(def_id);
        if normal_mod_id == def_id {
            self.opt_local_parent(def_id).map_or(Visibility::Public, Visibility::Restricted)
        } else {
            Visibility::Restricted(normal_mod_id)
        }
    }
}

impl<'a, 'b, 'tcx> IntoDefIdTree for &'b mut Resolver<'a, 'tcx> {
    type Tree = &'b Resolver<'a, 'tcx>;
    fn tree(self) -> Self::Tree {
        self
    }
}

impl<'r, 'a, 'tcx> EffectiveVisibilitiesVisitor<'r, 'a, 'tcx> {
    /// Fills the `Resolver::effective_visibilities` table with public & exported items
    /// For now, this doesn't resolve macros (FIXME) and cannot resolve Impl, as we
    /// need access to a TyCtxt for that.
    pub(crate) fn compute_effective_visibilities<'c>(
        r: &'r mut Resolver<'a, 'tcx>,
        krate: &'c Crate,
    ) {
        let mut visitor = EffectiveVisibilitiesVisitor {
            r,
            def_effective_visibilities: Default::default(),
            import_effective_visibilities: Default::default(),
            current_private_vis: Visibility::Public,
            changed: false,
        };

        visitor.update(CRATE_DEF_ID, CRATE_DEF_ID);
        visitor.current_private_vis = Visibility::Restricted(CRATE_DEF_ID);
        visitor.set_bindings_effective_visibilities(CRATE_DEF_ID);

        while visitor.changed {
            visitor.changed = false;
            visit::walk_crate(&mut visitor, krate);
        }
        visitor.r.effective_visibilities = visitor.def_effective_visibilities;

        // Update visibilities for import def ids. These are not used during the
        // `EffectiveVisibilitiesVisitor` pass, because we have more detailed binding-based
        // information, but are used by later passes. Effective visibility of an import def id
        // is the maximum value among visibilities of bindings corresponding to that def id.
        for (binding, eff_vis) in visitor.import_effective_visibilities.iter() {
            let NameBindingKind::Import { import, .. } = binding.kind else { unreachable!() };
            if let Some(node_id) = import.id() {
                r.effective_visibilities.update_eff_vis(
                    r.local_def_id(node_id),
                    eff_vis,
                    ResolverTree(&r.untracked),
                )
            }
        }

        info!("resolve::effective_visibilities: {:#?}", r.effective_visibilities);
    }

    /// Update effective visibilities of bindings in the given module,
    /// including their whole reexport chains.
    fn set_bindings_effective_visibilities(&mut self, module_id: LocalDefId) {
        assert!(self.r.module_map.contains_key(&&module_id.to_def_id()));
        let module = self.r.get_module(module_id.to_def_id()).unwrap();
        let resolutions = self.r.resolutions(module);

        for (_, name_resolution) in resolutions.borrow().iter() {
            if let Some(mut binding) = name_resolution.borrow().binding() && !binding.is_ambiguity() {
                // Set the given effective visibility level to `Level::Direct` and
                // sets the rest of the `use` chain to `Level::Reexported` until
                // we hit the actual exported item.
                let mut parent_id = ParentId::Def(module_id);
                while let NameBindingKind::Import { binding: nested_binding, .. } = binding.kind {
                    let binding_id = ImportId::new_unchecked(binding);
                    self.update_import(binding_id, parent_id);

                    parent_id = ParentId::Import(binding_id);
                    binding = nested_binding;
                }

                if let Some(def_id) = binding.res().opt_def_id().and_then(|id| id.as_local()) {
                    self.update_def(def_id, binding.vis.expect_local(), parent_id);
                }
            }
        }
    }

    fn cheap_private_vis(&self, parent_id: ParentId<'_>) -> Option<Visibility> {
        matches!(parent_id, ParentId::Def(_)).then_some(self.current_private_vis)
    }

    fn effective_vis_or_private(&mut self, parent_id: ParentId<'a>) -> EffectiveVisibility {
        // Private nodes are only added to the table for caching, they could be added or removed at
        // any moment without consequences, so we don't set `changed` to true when adding them.
        *match parent_id {
            ParentId::Def(def_id) => self
                .def_effective_visibilities
                .effective_vis_or_private(def_id, || self.r.private_vis_def(def_id)),
            ParentId::Import(binding) => self
                .import_effective_visibilities
                .effective_vis_or_private(binding, || self.r.private_vis_import(binding)),
        }
    }

    fn update_import(&mut self, binding: ImportId<'a>, parent_id: ParentId<'a>) {
        let nominal_vis = binding.vis.expect_local();
        let private_vis = self.cheap_private_vis(parent_id);
        let inherited_eff_vis = self.effective_vis_or_private(parent_id);
        self.changed |= self.import_effective_visibilities.update(
            binding,
            nominal_vis,
            |r| (private_vis.unwrap_or_else(|| r.private_vis_import(binding)), r),
            inherited_eff_vis,
            parent_id.level(),
            &mut *self.r,
        );
    }

    fn update_def(&mut self, def_id: LocalDefId, nominal_vis: Visibility, parent_id: ParentId<'a>) {
        let private_vis = self.cheap_private_vis(parent_id);
        let inherited_eff_vis = self.effective_vis_or_private(parent_id);
        self.changed |= self.def_effective_visibilities.update(
            def_id,
            nominal_vis,
            |r| (private_vis.unwrap_or_else(|| r.private_vis_def(def_id)), r),
            inherited_eff_vis,
            parent_id.level(),
            &mut *self.r,
        );
    }

    fn update(&mut self, def_id: LocalDefId, parent_id: LocalDefId) {
        self.update_def(def_id, self.r.visibilities[&def_id], ParentId::Def(parent_id));
    }
}

impl<'r, 'ast, 'tcx> Visitor<'ast> for EffectiveVisibilitiesVisitor<'ast, 'r, 'tcx> {
    fn visit_item(&mut self, item: &'ast ast::Item) {
        let def_id = self.r.local_def_id(item.id);
        // Update effective visibilities of nested items.
        // If it's a mod, also make the visitor walk all of its items
        match item.kind {
            // Resolved in rustc_privacy when types are available
            ast::ItemKind::Impl(..) => return,

            // Should be unreachable at this stage
            ast::ItemKind::MacCall(..) => panic!(
                "ast::ItemKind::MacCall encountered, this should not anymore appear at this stage"
            ),

            ast::ItemKind::Mod(..) => {
                let prev_private_vis =
                    mem::replace(&mut self.current_private_vis, Visibility::Restricted(def_id));
                self.set_bindings_effective_visibilities(def_id);
                visit::walk_item(self, item);
                self.current_private_vis = prev_private_vis;
            }

            ast::ItemKind::Enum(EnumDef { ref variants }, _) => {
                self.set_bindings_effective_visibilities(def_id);
                for variant in variants {
                    let variant_def_id = self.r.local_def_id(variant.id);
                    for field in variant.data.fields() {
                        self.update(self.r.local_def_id(field.id), variant_def_id);
                    }
                }
            }

            ast::ItemKind::Struct(ref def, _) | ast::ItemKind::Union(ref def, _) => {
                for field in def.fields() {
                    self.update(self.r.local_def_id(field.id), def_id);
                }
            }

            ast::ItemKind::Trait(..) => {
                self.set_bindings_effective_visibilities(def_id);
            }

            ast::ItemKind::ExternCrate(..)
            | ast::ItemKind::Use(..)
            | ast::ItemKind::Static(..)
            | ast::ItemKind::Const(..)
            | ast::ItemKind::GlobalAsm(..)
            | ast::ItemKind::TyAlias(..)
            | ast::ItemKind::TraitAlias(..)
            | ast::ItemKind::MacroDef(..)
            | ast::ItemKind::ForeignMod(..)
            | ast::ItemKind::Fn(..) => return,
        }
    }
}
