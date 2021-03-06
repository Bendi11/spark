//! Generating LLVM IR from a parsed and type lowered AST

pub mod astgen;
pub mod bingen;

use std::convert::TryFrom;

use codespan_reporting::diagnostic::{Diagnostic, Label};
use hashbrown::HashMap;
use inkwell::{
    basic_block::BasicBlock,
    builder::Builder,
    context::Context,
    module::{Linkage, Module},
    targets::{CodeModel, InitializationConfig, RelocMode, Target, TargetMachine},
    types::{AnyTypeEnum, BasicType, BasicTypeEnum, FunctionType as InkwellFunctionType, BasicMetadataTypeEnum},
    values::{BasicValueEnum, FunctionValue, PointerValue},
    AddressSpace, OptimizationLevel,
};
use quickscope::ScopeMap;
use hashbrown::HashSet;
use crate::{
    ast::{FunFlags, IntegerWidth, SymbolPath},
    codegen::ir::{FunId, FunctionType, ModId, SparkCtx, SparkDef, TypeData, TypeId},
    error::DiagnosticManager,
    util::{
        files::{FileId, Files},
        loc::Span,
    },
    CompileOpts, OutputOptimizationLevel, Symbol,
};

use super::CompilerRes;

/// A type representing all types that can be defined in the global scope
/// map of the code generator
#[derive(Clone, Copy)]
enum ScopeDef<'ctx> {
    Value(TypeId, PointerValue<'ctx>),
    Def(SparkDef),
}

/// Structure that generates LLVM IR modules from a parsed and
/// type lowered AST module
pub struct LlvmCodeGenerator<'ctx, 'files> {
    pub ctx: &'ctx Context,
    pub builder: Builder<'ctx>,
    pub spark: SparkCtx,
    pub diags: DiagnosticManager<'files>,
    pub opts: CompileOpts,
    /// The currently compiled file
    pub file: FileId,
    llvm_funs: HashMap<FunId, FunctionValue<'ctx>>,
    target: TargetMachine,
    current_scope: ScopeMap<Symbol, ScopeDef<'ctx>>,
    current_fun: Option<(FunctionValue<'ctx>, FunId)>,
    phi_data: Option<PhiData<'ctx>>,
    continue_bb: Option<BasicBlock<'ctx>>,
    break_bb: Option<BasicBlock<'ctx>>,
    placed_terminator: bool,
    codegened_funs: HashSet<FunId>,
}

/// Data needed to use a phi / break / continue statement
#[derive(Clone, Copy)]
struct PhiData<'ctx> {
    pub break_bb: BasicBlock<'ctx>,
    pub alloca: PointerValue<'ctx>,
    pub phi_ty: TypeId,
}

impl<'ctx, 'files> LlvmCodeGenerator<'ctx, 'files> {
    /// Create a new code generator from an LLVM context
    pub fn new(
        spark: SparkCtx,
        ctx: &'ctx Context,
        files: &'files Files,
        opts: CompileOpts,
    ) -> Self {
        Target::initialize_native(&InitializationConfig::default())
            .expect("LLVM: failed to initialize native compilation target");

        Self {
            current_scope: ScopeMap::new(),
            current_fun: None,
            builder: ctx.create_builder(),
            ctx,
            spark,
            file: unsafe { FileId::from_raw(0) },
            diags: DiagnosticManager::new(files),
            llvm_funs: HashMap::new(),
            phi_data: None,
            break_bb: None,
            continue_bb: None,
            placed_terminator: false,
            target: Target::from_triple(&TargetMachine::get_default_triple())
                .unwrap()
                .create_target_machine(
                    &TargetMachine::get_default_triple(),
                    TargetMachine::get_host_cpu_name().to_str().unwrap(),
                    TargetMachine::get_host_cpu_features().to_str().unwrap(),
                    match opts.opt_lvl {
                        OutputOptimizationLevel::Size => OptimizationLevel::Less,
                        OutputOptimizationLevel::Medium => OptimizationLevel::Less,
                        OutputOptimizationLevel::Debug => OptimizationLevel::None,
                        OutputOptimizationLevel::Release => OptimizationLevel::Aggressive,
                    },
                    match opts.pic {
                        true => RelocMode::PIC,
                        false => RelocMode::Default,
                    },
                    match opts.opt_lvl {
                        OutputOptimizationLevel::Size => CodeModel::Small,
                        _ => CodeModel::Default,
                    },
                )
                .unwrap(),
            opts,
            codegened_funs: HashSet::new()
        }
    }

    /// Find a name in the current scope
    fn find_in_scope(
        &self,
        span: Span,
        path: &SymbolPath,
    ) -> CompilerRes<ScopeDef<'ctx>> {
        let mut iter = path.iter();

        let first = iter.next().unwrap();

        match self.current_scope.get(&first) {
            Some(def) => {
                if iter.len() == 0 {
                    Ok(*def)
                } else {
                    match *def {
                        ScopeDef::Def(SparkDef::ModDef(submod)) => self
                            .spark
                            .get_def_impl(submod, iter)
                            .map(|d| ScopeDef::Def(d))
                            .map_err(|name| {
                                Diagnostic::error()
                                    .with_message(format!("'{}' not found in current scope", name))
                                    .with_labels(vec![Label::primary(self.file, span)])
                            }),
                        _ => Err(Diagnostic::error()
                            .with_message(format!(
                                "Cannot access '{}' of non-module definition",
                                iter.map(|s| s.as_str().to_owned())
                                    .collect::<Vec<_>>()
                                    .join(":")
                            ))
                            .with_labels(vec![Label::primary(self.file, span)])),
                    }
                }
            }
            None => Err(Diagnostic::error()
                .with_message(format!("Symbol '{}' not found in the current scope", first))
                .with_labels(vec![Label::primary(self.file, span)])),
        }
    }
    
    /// Generate code for definitions
    fn codegen_defs(&mut self, module: ModId) {
        let defs = self.spark[module].defs.clone();

        self.current_scope.push_layer();

        for (name, def) in defs.iter() {
            self.current_scope.define(name.clone(), ScopeDef::Def(*def));
        }

        let defs = self.spark[module].defs.clone();
        for (name, def) in defs.iter() {
            if let SparkDef::FunDef(file, fun) = def {
                self.file = *file;
                if let Some(ref body) = self.spark[*fun].body {
                    self.placed_terminator = false;
                    let llvm_fun = *self.llvm_funs.get(fun).unwrap();
                    let entry = self.ctx.append_basic_block(llvm_fun, "entry_bb");
                    self.builder.position_at_end(entry);

                    self.current_fun = Some((llvm_fun, *fun));
                    self.current_scope.push_layer();
                    for (arg, (arg_name, arg_ty)) in self.llvm_funs[fun].get_param_iter()
                        .zip(self.spark[*fun].arg_names.iter().zip(self.spark[*fun].ty.args.iter())) {
                        if let Some(arg_name) = arg_name {
                            let arg_alloca = self.builder.build_alloca(arg.get_type(), "arg_alloca");
                            self.builder.build_store(arg_alloca, arg);
                            self.current_scope.define(*arg_name, ScopeDef::Value(*arg_ty, arg_alloca));
                        }
                    }

                    for stmt in body.clone() {
                        if let Err(e) = self.gen_stmt(module, &stmt) {
                            self.diags
                                .emit(e.with_notes(vec![format!("In function {}", name)]));
                        }
                    }
                    self.current_scope.pop_layer();
                }
            }
        }

        self.current_scope.pop_layer();

        for (_name, def) in defs.iter() {
            if let SparkDef::ModDef(submod) = def {
                self.codegen_defs(*submod);
            }
        }
    }

    /// Codegen LLVM IR from a type-lowered module
    pub fn codegen_module(&mut self, module: ModId) -> CompilerRes<Module<'ctx>> {
        let mut llvm_mod = self.ctx.create_module(self.spark[module].name.as_str());
        if let Err(e) = self.forward_funs(module, &mut llvm_mod) {
            self.diags.emit(e.clone());
            return Err(e)
        }
        self.codegen_defs(module);
        Ok(llvm_mod)
    }

    /// Generate code for all function prototypes
    fn forward_funs(&mut self, module: ModId, llvm: &mut Module<'ctx>) -> CompilerRes<()> {
        let defs = self.spark[module].defs.clone();

        for fun_id in defs.iter().filter_map(|(_, def)| {
            if let SparkDef::FunDef(_, id) = def {
                Some(*id)
            } else {
                None
            }
        }) {
            if self.codegened_funs.contains(&fun_id) {
                    continue
            }
            self.codegened_funs.insert(fun_id);
            let fun = self.spark[fun_id].clone();
            let llvm_fun_ty = self.gen_fun_ty(fun.span, &fun.ty)?;
            let llvm_fun = if fun.flags.contains(FunFlags::EXTERN) {
                llvm.add_function(fun.name.as_str(), llvm_fun_ty, Some(Linkage::External))
            } else {
                llvm.add_function(
                    format!("{}-{}", fun.name, uuid::Uuid::new_v4()).as_str(),
                    llvm_fun_ty,
                    Some(Linkage::Internal),
                )
            };
            self.llvm_funs.insert(fun_id, llvm_fun);
        }

        for child in defs.iter() {
            if let SparkDef::ModDef(child) = child.1 {
                self.forward_funs(*child, llvm)?;
            }
        }

        Ok(())
    }

    /// Create an LLVM type from a type ID
    fn llvm_ty(&mut self, span: Span, id: TypeId) -> CompilerRes<AnyTypeEnum<'ctx>> {
        Ok(match self.spark[id].clone() {
            TypeData::Integer { signed: _, width } => match width {
                IntegerWidth::Eight => self.ctx.i8_type().into(),
                IntegerWidth::Sixteen => self.ctx.i16_type().into(),
                IntegerWidth::ThirtyTwo => self.ctx.i32_type().into(),
                IntegerWidth::SixtyFour => self.ctx.i64_type().into(),
            },
            TypeData::Bool => self.ctx.bool_type().into(),
            TypeData::Struct { fields } => {
                let fields = fields
                    .iter()
                    .map(|(id, _)| match self.llvm_ty(span, *id) {
                        Ok(ty) => Ok(BasicTypeEnum::try_from(ty).ok()),
                        Err(e) => Err(e)
                    })
                    .filter_map(|i| match i {
                        Ok(Some(e)) => Some(Ok(e)),
                        Err(e) => Some(Err(e)),
                        _ => None
                    }) //Filter None out
                    .collect::<Result<Vec<_>, _>>()?;
                self.ctx.struct_type(&fields, false).into()
            }
            TypeData::Alias(_, id) => self.llvm_ty(span, id)?,
            TypeData::Pointer(id) => {
                let pointee = Self::require_basictype(self.file, span, self.llvm_ty(span, id)?)?;

                pointee.ptr_type(AddressSpace::Generic).into()
            }
            TypeData::Array { element, len } => Self::require_basictype(self.file, span, self.llvm_ty(span, element)?)?
                .array_type(len as u32)
                .into(),
            TypeData::Unit => self.ctx.void_type().into(),
            TypeData::Invalid => unreachable!(),
            TypeData::Float { doublewide } => match doublewide {
                true => self.ctx.f64_type().into(),
                false => self.ctx.f32_type().into(),
            },
            TypeData::Function(ty) => self.gen_fun_ty(span, &ty)?.ptr_type(AddressSpace::Generic).into(),
            TypeData::Enum { parts } => {
                let max = parts
                    .iter()
                    .map(|part| self.size_of_type(*part))
                    .max()
                    .unwrap_or(0);

                if max > 0 {
                    self.ctx
                        .struct_type(
                            &[
                                self.ctx.i8_type().into(),
                                self.ctx.i8_type().array_type(max).into(),
                            ],
                            true,
                        )
                        .into()
                } else {
                    self.ctx
                        .struct_type(&[self.ctx.i8_type().into()], true)
                        .into()
                }
            }
        })
    }

    /// Create an LLVM function type from a spark IR function type
    fn gen_fun_ty(&mut self, span: Span, ty: &FunctionType) -> CompilerRes<InkwellFunctionType<'ctx>> {
        let return_ty = self.llvm_ty(span, ty.return_ty)?;
        let args = ty
            .args
            .iter()
            .map(|ty| match self.llvm_ty(span, *ty) {
                Ok(ty) => Self::require_basictype(self.file, span, ty).map(BasicMetadataTypeEnum::from),
                Err(e) => Err(e),
            }).collect::<Result<Vec<_>, _>>()?;
        
        Ok(match return_ty {
            AnyTypeEnum::VoidType(return_ty) => return_ty.fn_type(&args, false),
            _ => BasicTypeEnum::try_from(return_ty)
                .unwrap()
                .fn_type(&args, false),
        })
    }

    /// Get the size of a type in bytes from a type ID
    fn size_of_type(&self, ty: TypeId) -> u32 {
        match &self.spark[ty] {
            TypeData::Integer { width, .. } => (*width as u8 / 8) as u32,
            TypeData::Float { doublewide: true } => 8,
            TypeData::Float { doublewide: false } => 4,
            TypeData::Enum { parts } => self.biggest_size(parts),
            TypeData::Bool => 1,
            TypeData::Struct { fields } => {
                fields.iter().map(|field| self.size_of_type(field.0)).sum()
            }
            TypeData::Unit => 0,
            TypeData::Pointer(_) => self.ptr_size(),
            TypeData::Array { element, len } => self.size_of_type(*element) * *len as u32,
            TypeData::Alias(_, ty) => self.size_of_type(*ty),
            TypeData::Function(_) => self.ptr_size(),
            TypeData::Invalid => unreachable!(),
        }
    }
            
    /// Get the largest type of a list of types
    fn biggest_size(&self, types: &[TypeId]) -> u32 {
        types
            .iter()
            .map(|ty| self.size_of_type(*ty))
            .max()
            .unwrap_or(0)
    }

    /// Get the size in bytes of a pointer on the target platform
    fn ptr_size(&self) -> u32 {
        self.target.get_target_data().get_pointer_byte_size(None)
    }
    
    /// Require a given type to not be a zero-sized type
    fn require_basictype(file: FileId, span: Span, ty: AnyTypeEnum<'ctx>) -> CompilerRes<BasicTypeEnum<'ctx>> {
        BasicTypeEnum::try_from(ty)
            .map_err(|_| {
                Diagnostic::error()
                    .with_message("Cannot use zero-sized type here")
                    .with_labels(vec![
                        Label::primary(file, span)
                            .with_message("This is a zero-sized type")
                    ])
            })
    }
}
