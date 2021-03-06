// Copyright (c) The Diem Core Contributors
// SPDX-License-Identifier: Apache-2.0

use crate::{
    annotations::Annotations,
    borrow_analysis, livevar_analysis, reaching_def_analysis,
    stackless_bytecode::{AttrId, Bytecode, Operation, SpecBlockId, TempIndex},
};
use itertools::Itertools;
use move_model::{
    ast::{Exp, Spec},
    model::{FunId, FunctionEnv, GlobalEnv, Loc, ModuleEnv, QualifiedId, StructId, TypeParameter},
    symbol::{Symbol, SymbolPool},
    ty::{Type, TypeDisplayContext},
};
use std::{
    cell::RefCell,
    collections::{BTreeMap, BTreeSet},
    fmt,
};
use vm::file_format::CodeOffset;

/// A FunctionTarget is a drop-in replacement for a FunctionEnv which allows to rewrite
/// and analyze bytecode and parameter/local types. It encapsulates a FunctionEnv and information
/// which can be rewritten using the `FunctionTargetsHolder` data structure.
pub struct FunctionTarget<'env> {
    pub func_env: &'env FunctionEnv<'env>,
    pub data: &'env FunctionData,

    // Used for debugging and testing, containing any attached annotation formatters.
    annotation_formatters: RefCell<Vec<Box<AnnotationFormatter>>>,
}

impl<'env> Clone for FunctionTarget<'env> {
    fn clone(&self) -> Self {
        // Annotation formatters are transient and forgotten on clone, so this is a cheap
        // handle.
        Self {
            func_env: self.func_env,
            data: self.data,
            annotation_formatters: RefCell::new(vec![]),
        }
    }
}

/// Holds the owned data belonging to a FunctionTarget, contained in a
/// `FunctionTargetsHolder`.
#[derive(Debug)]
pub struct FunctionData {
    /// The bytecode.
    pub code: Vec<Bytecode>,
    /// The locals, including parameters.
    pub local_types: Vec<Type>,
    /// The return types.
    pub return_types: Vec<Type>,
    /// TODO(wrwg): document what this is for
    pub param_proxy_map: BTreeMap<usize, usize>,
    /// A map from mut ref input parameters to the generated output parameters.
    pub ref_param_proxy_map: BTreeMap<usize, usize>,
    /// A map from mut ref output parameters to the input parameters.
    pub ref_param_return_map: BTreeMap<usize, usize>,
    /// The set of global resources acquired by  this function.
    pub acquires_global_resources: Vec<StructId>,
    /// A map from byte code attribute to source code location.
    pub locations: BTreeMap<AttrId, Loc>,
    /// Annotations associated with this function.
    pub annotations: Annotations,
    /// Map of spec block ids as given by the source, to the code offset in the original
    /// bytecode. Those spec block's content is found at
    /// `func_env.get_specification_on_impl(code_offset)`.
    pub spec_blocks_on_impl: BTreeMap<SpecBlockId, CodeOffset>,
    /// A map from local names to temp indices in code.
    pub name_to_index: BTreeMap<Symbol, usize>,
    /// A cache of targets modified by this function.
    pub modify_targets: BTreeMap<QualifiedId<StructId>, Vec<Exp>>,
}

pub struct FunctionDataBuilder<'a> {
    pub data: &'a mut FunctionData,
    pub next_attr_index: usize,
}

impl<'env> FunctionTarget<'env> {
    pub fn new(
        func_env: &'env FunctionEnv<'env>,
        data: &'env FunctionData,
    ) -> FunctionTarget<'env> {
        FunctionTarget {
            func_env,
            data,
            annotation_formatters: RefCell::new(vec![]),
        }
    }

    /// Returns the name of this function.
    pub fn get_name(&self) -> Symbol {
        self.func_env.get_name()
    }

    /// Gets the id of this function.
    pub fn get_id(&self) -> FunId {
        self.func_env.get_id()
    }

    /// Shortcut for accessing the symbol pool.
    pub fn symbol_pool(&self) -> &SymbolPool {
        self.func_env.module_env.symbol_pool()
    }

    /// Shortcut for accessing the module env of this function.
    pub fn module_env(&self) -> &ModuleEnv {
        &self.func_env.module_env
    }

    /// Shortcut for accessing the global env of this function.
    pub fn global_env(&self) -> &GlobalEnv {
        self.func_env.module_env.env
    }

    /// Returns the location of this function.
    pub fn get_loc(&self) -> Loc {
        self.func_env.get_loc()
    }

    /// Returns the location of the bytecode with the given attribute.
    pub fn get_bytecode_loc(&self, attr_id: AttrId) -> Loc {
        if let Some(loc) = self.data.locations.get(&attr_id) {
            loc.clone()
        } else {
            self.get_loc()
        }
    }

    /// Returns true if this function is native.
    pub fn is_native(&self) -> bool {
        self.func_env.is_native()
    }

    /// Returns true if this function is marked as intrinsic
    pub fn is_intrinsic(&self) -> bool {
        self.func_env.is_intrinsic()
    }

    /// Returns true if this function is opaque.
    pub fn is_opaque(&self) -> bool {
        self.func_env.is_opaque()
    }

    /// Returns true if this function is public.
    pub fn is_public(&self) -> bool {
        self.func_env.is_public()
    }

    /// Returns true if this function mutates any references (i.e. has &mut parameters).
    pub fn is_mutating(&self) -> bool {
        self.func_env.is_mutating()
    }

    /// Returns the type parameters associated with this function.
    pub fn get_type_parameters(&self) -> Vec<TypeParameter> {
        self.func_env.get_type_parameters()
    }

    /// Returns return type at given index.
    pub fn get_return_type(&self, idx: usize) -> &Type {
        &self.data.return_types[idx]
    }

    /// Returns return types of this function.
    pub fn get_return_types(&self) -> &[Type] {
        &self.data.return_types
    }

    /// Returns the number of return values of this function.
    pub fn get_return_count(&self) -> usize {
        self.data.return_types.len()
    }

    pub fn get_parameter_count(&self) -> usize {
        self.func_env.get_parameter_count()
    }

    /// Get the name to be used for a local. If the local is an argument, use that for naming,
    /// otherwise generate a unique name.
    pub fn get_local_name(&self, idx: usize) -> Symbol {
        self.func_env.get_local_name(idx)
    }

    /// Get the index corresponding to a local name
    pub fn get_local_index(&self, name: Symbol) -> Option<usize> {
        self.data.name_to_index.get(&name).cloned().or_else(|| {
            // TODO(wrwg): remove this hack once we have Exp::Local using an index
            //   instead of a symbol.
            let str = self.global_env().symbol_pool().string(name);
            if let Some(s) = str.strip_prefix("$t") {
                Some(s.parse::<usize>().unwrap())
            } else {
                None
            }
        })
    }

    /// Gets the number of locals of this function, including parameters.
    pub fn get_local_count(&self) -> usize {
        self.data.local_types.len()
    }

    /// Gets the number of user declared locals of this function, excluding locals which have
    /// been introduced by transformations.
    pub fn get_user_local_count(&self) -> usize {
        self.func_env.get_local_count()
    }

    /// Returns true if the index is for a temporary, not user declared local.
    pub fn is_temporary(&self, idx: usize) -> bool {
        self.func_env.is_temporary(idx)
    }

    /// Gets the type of the local at index. This must use an index in the range as determined by
    /// `get_local_count`.
    pub fn get_local_type(&self, idx: usize) -> &Type {
        &self.data.local_types[idx]
    }

    /// Returns specification associated with this function.
    pub fn get_spec(&'env self) -> &'env Spec {
        self.func_env.get_spec()
    }

    /// Returns specification conditions associated with this function at spec block id.
    pub fn get_spec_on_impl(&'env self, block_id: SpecBlockId) -> &'env Spec {
        let code_offset = self
            .data
            .spec_blocks_on_impl
            .get(&block_id)
            .expect("spec block defined");
        self.func_env
            .get_spec()
            .on_impl
            .get(code_offset)
            .expect("given spec block defined")
    }

    /// Returns the value of a boolean pragma for this function. This first looks up a
    /// pragma in this function, then the enclosing module, and finally uses the provided default.
    /// property
    pub fn is_pragma_true(&self, name: &str, default: impl FnOnce() -> bool) -> bool {
        self.func_env.is_pragma_true(name, default)
    }

    /// Gets the bytecode.
    pub fn get_bytecode(&self) -> &[Bytecode] {
        &self.data.code
    }

    /// Gets annotations.
    pub fn get_annotations(&self) -> &Annotations {
        &self.data.annotations
    }

    /// Gets acquired resources
    pub fn get_acquires_global_resources(&self) -> &[StructId] {
        &self.data.acquires_global_resources
    }

    /// Gets index of return parameter for a reference input parameter
    pub fn get_return_index(&self, idx: usize) -> Option<&usize> {
        self.data.ref_param_return_map.get(&idx)
    }

    /// For a return index, return the reference input parameter. Inverse of
    /// `get_return_index`.
    pub fn get_input_for_return_index(&self, idx: usize) -> Option<&usize> {
        // We do a brute force linear search. This may need to be changed if we are dealing
        // with truly large (like generated) parameter lists.
        for (ref_idx, ret_idx) in &self.data.ref_param_return_map {
            if *ret_idx == idx {
                return Some(ref_idx);
            }
        }
        None
    }

    /// TODO(wrwg): better document what this does, it seems to be related to loop invariants.
    pub fn get_proxy_index(&self, idx: usize) -> Option<&usize> {
        self.data.param_proxy_map.get(&idx)
    }

    /// Gets index of mutable proxy variable for an input ref parameter
    pub fn get_ref_proxy_index(&self, idx: usize) -> Option<&usize> {
        self.data.ref_param_proxy_map.get(&idx)
    }

    /// Reverse of `get_ref_proxy_index`.
    pub fn get_reverse_ref_proxy_index(&self, idx: usize) -> Option<&usize> {
        // We do a brute force linear search.
        for (ref_idx, proxy_idx) in &self.data.ref_param_proxy_map {
            if *proxy_idx == idx {
                return Some(ref_idx);
            }
        }
        None
    }

    /// Returns true if this is an unchecked parameter. Such a parameter (currently) stems
    /// from a `&mut` parameter in Move which has been converted to in/out parameters in the
    /// transformation pipeline, provided this is a private function.
    pub fn is_unchecked_param(&self, idx: TempIndex) -> bool {
        (!self.is_public() || !self.call_ends_lifetime()) && self.get_ref_proxy_index(idx).is_some()
    }

    /// Returns whether a call to this function ends lifetime of input references
    pub fn call_ends_lifetime(&self) -> bool {
        self.is_public() && self.get_return_types().iter().all(|ty| !ty.is_reference())
    }

    /// Gets modify targets for a type
    pub fn get_modify_targets_for_type(&self, ty: &QualifiedId<StructId>) -> Option<&Vec<Exp>> {
        self.get_modify_targets().get(ty)
    }

    /// Gets all modify targets
    pub fn get_modify_targets(&self) -> &BTreeMap<QualifiedId<StructId>, Vec<Exp>> {
        &self.data.modify_targets
    }
}

impl FunctionData {
    /// Creates new function target data.
    pub fn new(
        func_env: &FunctionEnv<'_>,
        code: Vec<Bytecode>,
        local_types: Vec<Type>,
        return_types: Vec<Type>,
        locations: BTreeMap<AttrId, Loc>,
        acquires_global_resources: Vec<StructId>,
        given_spec_blocks: BTreeMap<SpecBlockId, CodeOffset>,
    ) -> Self {
        let name_to_index = (0..func_env.get_local_count())
            .map(|idx| (func_env.get_local_name(idx), idx))
            .collect();
        let modify_targets = func_env.get_modify_targets();
        FunctionData {
            code,
            local_types,
            return_types,
            param_proxy_map: Default::default(),
            ref_param_proxy_map: Default::default(),
            ref_param_return_map: Default::default(),
            acquires_global_resources,
            locations,
            annotations: Default::default(),
            spec_blocks_on_impl: given_spec_blocks,
            name_to_index,
            modify_targets,
        }
    }

    /// Computes the next available index for AttrId.
    pub fn next_free_attr_index(&self) -> usize {
        self.code
            .iter()
            .map(|b| b.get_attr_id().as_usize())
            .max()
            .unwrap_or(0)
            + 1
    }

    /// Computes the next available index for Label.
    pub fn next_free_label_index(&self) -> usize {
        self.code
            .iter()
            .filter_map(|b| {
                if let Bytecode::Label(_, l) = b {
                    Some(l.as_usize())
                } else {
                    None
                }
            })
            .max()
            .unwrap_or(0)
            + 1
    }

    /// Return the set of callees invoked by this function, including native functions
    pub fn get_callees(&self) -> BTreeSet<QualifiedId<FunId>> {
        use Bytecode::*;
        use Operation::*;

        let mut callees = BTreeSet::new();
        for instr in &self.code {
            if let Call(_, _, Function(mid, fid, _), _) = instr {
                let callee = mid.qualified(*fid);
                callees.insert(callee);
            }
        }
        callees
    }

    /// Apply a variable renaming to this data, adjusting internal data structures.
    pub fn rename_vars<F>(&mut self, f: &F)
    where
        F: Fn(TempIndex) -> TempIndex,
    {
        self.param_proxy_map = std::mem::take(&mut self.param_proxy_map)
            .into_iter()
            .map(|(x, y)| (f(x), f(y)))
            .collect();
        self.ref_param_proxy_map = std::mem::take(&mut self.ref_param_proxy_map)
            .into_iter()
            .map(|(x, y)| (f(x), f(y)))
            .collect();
    }
}

impl Clone for FunctionData {
    /// Create a clone of this function data, without annotations.
    fn clone(&self) -> Self {
        FunctionData {
            code: self.code.clone(),
            local_types: self.local_types.clone(),
            return_types: self.return_types.clone(),
            param_proxy_map: self.param_proxy_map.clone(),
            ref_param_proxy_map: self.ref_param_proxy_map.clone(),
            ref_param_return_map: self.ref_param_return_map.clone(),
            acquires_global_resources: self.acquires_global_resources.clone(),
            locations: self.locations.clone(),
            annotations: Default::default(),
            spec_blocks_on_impl: self.spec_blocks_on_impl.clone(),
            name_to_index: self.name_to_index.clone(),
            modify_targets: self.modify_targets.clone(),
        }
    }
}

// =================================================================================================
// Formatting

/// A function which is called to display the value of an annotation for a given function target
/// at the given code offset. The function is passed the function target and the code offset, and
/// is expected to pick the annotation of its respective type from the function target and for
/// the given code offset. It should return None if there is no relevant annotation.
pub type AnnotationFormatter = dyn Fn(&FunctionTarget<'_>, CodeOffset) -> Option<String>;

impl<'env> FunctionTarget<'env> {
    /// Register a formatter. Each function target processor which introduces new annotations
    /// should register a formatter in order to get is value printed when a function target
    /// is displayed for debugging or testing.
    pub fn register_annotation_formatter(&self, formatter: Box<AnnotationFormatter>) {
        self.annotation_formatters.borrow_mut().push(formatter);
    }

    /// Tests use this function to register all relevant annotation formatters. Extend this with
    /// new formatters relevant for tests.
    pub fn register_annotation_formatters_for_test(&self) {
        self.register_annotation_formatter(Box::new(livevar_analysis::format_livevar_annotation));
        self.register_annotation_formatter(Box::new(borrow_analysis::format_borrow_annotation));
        self.register_annotation_formatter(Box::new(
            reaching_def_analysis::format_reaching_def_annotation,
        ));
    }
}

impl<'env> fmt::Display for FunctionTarget<'env> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}fun {}::{}",
            if self.is_public() { "pub " } else { "" },
            self.func_env
                .module_env
                .get_name()
                .display(self.symbol_pool()),
            self.get_name().display(self.symbol_pool())
        )?;
        let tparams = &self.get_type_parameters();
        if !tparams.is_empty() {
            write!(f, "<")?;
            for (i, TypeParameter(name, _)) in tparams.iter().enumerate() {
                if i > 0 {
                    write!(f, ", ")?;
                }
                write!(f, "{}", name.display(self.symbol_pool()))?;
            }
            write!(f, ">")?;
        }
        let tctx = TypeDisplayContext::WithEnv {
            env: self.global_env(),
            type_param_names: None,
        };
        write!(f, "(")?;
        for i in 0..self.get_parameter_count() {
            if i > 0 {
                write!(f, ", ")?;
            }
            write!(
                f,
                "{}: {}",
                self.get_local_name(i).display(self.symbol_pool()),
                self.get_local_type(i).display(&tctx)
            )?;
        }
        write!(f, ")")?;
        if self.get_return_count() > 0 {
            write!(f, ": ")?;
            if self.get_return_count() > 1 {
                write!(f, "(")?;
            }
            for i in 0..self.get_return_count() {
                if i > 0 {
                    write!(f, ", ")?;
                }
                write!(f, "{}", self.get_return_type(i).display(&tctx))?;
            }
            if self.get_return_count() > 1 {
                write!(f, ")")?;
            }
        }
        writeln!(f, " {{")?;
        for i in self.get_parameter_count()..self.get_local_count() {
            writeln!(
                f,
                "     var {}: {}",
                self.get_local_name(i).display(self.symbol_pool()),
                self.get_local_type(i).display(&tctx)
            )?;
        }
        let mut loc_vc_shown = BTreeSet::new();
        for (offset, code) in self.get_bytecode().iter().enumerate() {
            let annotations = self
                .annotation_formatters
                .borrow()
                .iter()
                .filter_map(|f| f(self, offset as CodeOffset))
                .map(|s| format!("     // {}", s.replace("\n", "\n     // ")))
                .join("\n");
            if !annotations.is_empty() {
                writeln!(f, "{}", annotations)?;
            }
            if let Some(loc) = self.data.locations.get(&code.get_attr_id()) {
                if matches!(code, Bytecode::Prop(..)) && loc_vc_shown.insert(loc.clone()) {
                    for (tag, info) in self.func_env.module_env.env.get_condition_infos(loc) {
                        writeln!(f, "     // VC: {} for {}", info, tag)?;
                    }
                }
            }
            writeln!(f, "{:>3}: {}", offset, code.display(self))?;
        }
        writeln!(f, "}}")?;
        Ok(())
    }
}
