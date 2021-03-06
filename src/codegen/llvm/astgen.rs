use codespan_reporting::diagnostic::{Diagnostic, Label};
use inkwell::{types::IntType, values::CallableValue, FloatPredicate, IntPredicate};

use crate::{
    ast::{Ast, AstNode, ElseExpr, IfExpr, Literal, NumberLiteral, NumberLiteralAnnotation},
    parse::token::Op,
    util::files::FileId, codegen::CompilerRes,
};

use super::*;

impl<'ctx, 'files> LlvmCodeGenerator<'ctx, 'files> {
    /// Generate code for a single AST statement
    pub fn gen_stmt(
        &mut self,
        module: ModId,
        ast: &Ast<TypeId>,
    ) -> CompilerRes<()> {
        match &ast.node {
            AstNode::Block(block) => {
                self.gen_block_ast(module, block)?;
            }
            AstNode::IfExpr(if_expr) => {
                self.gen_if_expr(module, if_expr)?;
            }
            AstNode::FunCall(called, args) => {
                self.gen_call( module, called, args)?;
            }
            AstNode::Match { matched, cases } => {
                self.gen_match_expr(module, matched, cases, ast.span)?;
            }
            AstNode::Assignment { lhs, rhs } => {
                let rhs_ty = self.ast_type(module, rhs)?;

                let lhs_ty = if let AstNode::VarDeclaration { ty: None, .. } = &lhs.node {
                    rhs_ty
                } else {
                    self.ast_type(module, lhs)?
                };
                if lhs_ty != rhs_ty {
                    return Err(Diagnostic::error()
                        .with_message(format!(
                            "Value of type {} cannot be assigned to type of {}",
                            self.spark.get_type_name(rhs_ty),
                            self.spark.get_type_name(lhs_ty),
                        ))
                        .with_labels(vec![
                            Label::primary(self.file, ast.span)
                                .with_message("Assignee encountered here"),
                            Label::secondary(self.file, ast.span)
                                .with_message("Assigned value encountered here"),
                        ]));
                }

                let lhs = if let AstNode::VarDeclaration {
                    name,
                    ty: _,
                    mutable: _,
                } = &lhs.node
                {
                    let llvm_ty = Self::require_basictype(self.file, ast.span, self.llvm_ty(ast.span, lhs_ty)?)?;
                    if let Ok(llvm_ty) = BasicTypeEnum::try_from(llvm_ty) {
                        let pv = self.builder.build_alloca(llvm_ty, "var_dec_aloca");
                        self.current_scope
                            .define(*name, ScopeDef::Value(lhs_ty, pv));
                        pv
                    } else {
                        return Err(Diagnostic::error()
                            .with_message(format!(
                                "Cannot declare a variable of type {}",
                                self.spark.get_type_name(lhs_ty)
                            ))
                            .with_labels(vec![Label::primary(self.file, lhs.span)]));
                    }
                } else {
                    self.gen_lval(module, lhs)?
                };

                let rhs = self.gen_expr(module, rhs)?;

                self.builder.build_store(lhs, rhs);
            }
            AstNode::VarDeclaration { name, ty, mutable } => {
                if let Some(ty) = ty {
                    let llvm_ty = self.llvm_ty(ast.span, *ty)?;
                    if let Ok(llvm_ty) = BasicTypeEnum::try_from(llvm_ty) {
                        let pv = self.builder.build_alloca(llvm_ty, name.as_str());
                        self.current_scope
                            .define(*name, ScopeDef::Value(*ty, pv.into()));
                    } else {
                        return Err(Diagnostic::error()
                            .with_message("Cannot declare variable of unit type")
                            .with_labels(vec![Label::primary(self.file, ast.span)]));
                    }
                } else {
                    return Err(Diagnostic::error()
                        .with_message("Must provide type of variable or assign a value")
                        .with_labels(vec![Label::primary(self.file, ast.span)
                            .with_message("In this variable declaration")])
                        .with_notes(vec![format!(
                            "Provide an explicit type in parenthesis after the '{}' keyword",
                            if *mutable { "mut" } else { "let " }
                        )]));
                }
            }
            AstNode::Return(returned) => {
                let returned_ty = self.ast_type(module, returned).map_err(|e| {
                    e.with_labels(vec![
                        Label::secondary(self.file, ast.span).with_message("In this return statement")
                    ])
                })?;

                let current_fun = &self.spark[self.current_fun.unwrap().1];

                if returned_ty != current_fun.ty.return_ty {
                    return Err(Diagnostic::error()
                        .with_message(format!(
                                "Returned value of type '{}' is not compatible with declared return type of '{}'",
                                self.spark.get_type_name(returned_ty),
                                self.spark.get_type_name(current_fun.ty.return_ty),
                            )
                        )
                    );
                }

                self.placed_terminator = true;

                if current_fun.ty.return_ty != SparkCtx::UNIT {
                    let returned = self.gen_expr(module, returned)?;
                    self.builder.build_return(Some(&returned));
                } else {
                    self.builder.build_return(None);
                }
            }
            AstNode::PhiExpr(phi) => {
                if let Some(phi_data) = self.phi_data {
                    let phid_ty = self.ast_type(module, phi)?;

                    if phid_ty != phi_data.phi_ty {
                        return Err(Diagnostic::error()
                            .with_message("Phi statement returns a value with type different to expected type")
                            .with_labels(vec![
                                Label::primary(self.file, phi.span)
                                    .with_message(format!("Phi statement of type '{}' encountered here", self.spark.get_type_name(phid_ty)))
                            ])
                        );
                    }

                    let phi_val = self.gen_expr(module, phi)?;
                    self.builder.build_store(phi_data.alloca, phi_val);
                    self.placed_terminator = true;

                    self.builder.build_unconditional_branch(phi_data.break_bb);
                } else {
                    return Err(Diagnostic::error()
                        .with_message("Phi statement not in a block")
                        .with_labels(vec![Label::primary(self.file, ast.span)]));
                }
            }
            AstNode::Break => {
                if let Some(break_bb) = self.break_bb {
                    self.placed_terminator = true;
                    self.builder.build_unconditional_branch(break_bb);
                } else {
                    return Err(Diagnostic::error()
                        .with_message("Break statement encountered while not in a block")
                        .with_labels(vec![Label::primary(self.file, ast.span)]));
                }
            }
            AstNode::Continue => {
                if let Some(continue_bb) = self.continue_bb {
                    self.placed_terminator = true;
                    self.builder.build_unconditional_branch(continue_bb);
                } else {
                    return Err(Diagnostic::error()
                        .with_message("Continue statement while not in a block")
                        .with_labels(vec![Label::primary(self.file, ast.span)
                            .with_message("Continue statement encountered here")]));
                }
            }
            other => {
                return Err(Diagnostic::error()
                    .with_message(format!("Invalid statement: {:#?}", other))
                    .with_labels(vec![Label::primary(self.file, ast.span)]))
            }
        }

        Ok(())
    }

    /// Generate code for a single AST expression
    fn gen_expr(
        &mut self,
        module: ModId,
        ast: &Ast<TypeId>,
    ) -> CompilerRes<BasicValueEnum<'ctx>> {
        Ok(match &ast.node {
            AstNode::IfExpr(..) | AstNode::Block(..) | AstNode::Match { .. } => {
                let phi = self.gen_lval(module, ast)?;
                self.builder.build_load(phi, "load_phi")
            }
            AstNode::MemberAccess(object, field) => {
                let field_pv = self.gen_member(module, object, *field)?;
                self.builder.build_load(field_pv, "load_struct_member")
            }
            AstNode::CastExpr(to, rhs) => self.gen_cast(module, *to, rhs)?,
            AstNode::Access(path) => {
                let access = self.gen_access(ast.span, path)?;
                if access.get_type().get_element_type().is_function_type() {
                    access.into()
                } else {
                    self.builder.build_load(access, "var_rval_load").into()
                }
            }
            AstNode::UnaryExpr(op, rhs) => {
                let rhs_ty = self.ast_type(module, rhs)?;
                match op {
                    Op::AND => {
                        let lval = self.gen_lval(module, rhs)?;
                        lval.into()
                    }
                    Op::Star => {
                        if let TypeData::Pointer(_) = &self.spark[rhs_ty] {
                            let pv = self.gen_expr(module, rhs)?.into_pointer_value();
                            let deref = self.builder.build_load(pv, "deref_load");
                            deref
                        } else {
                            return Err(Diagnostic::error()
                                .with_message(format!(
                                    "Expression of type {} cannot be dereferenced",
                                    self.spark.get_type_name(rhs_ty),
                                ))
                                .with_labels(vec![Label::primary(self.file, ast.span)]));
                        }
                    }
                    _ => {
                        return Err(Diagnostic::error()
                            .with_message(format!("Invalid unary operand {}", op))
                            .with_labels(vec![Label::primary(self.file, ast.span)]))
                    }
                }
            }
            AstNode::FunCall(called, args) => match self.gen_call(module, called, args)? {
                Some(v) => v,
                None => {
                    return Err(Diagnostic::error()
                        .with_message("Cannot use function returning unit type as an expression")
                        .with_labels(vec![Label::primary(self.file, called.span)
                            .with_message("This is found to be of function type returning '()'")]))
                }
            },
            AstNode::BinExpr(lhs, op, rhs) => {
                return self.gen_bin_expr(module, lhs, *op, rhs)
            }
            AstNode::Literal(literal) => self.gen_literal(module, literal, ast.span)?,
            _ => {
                return Err(Diagnostic::error()
                    .with_message("Expression not yet implemented")
                    .with_labels(vec![Label::primary(self.file, ast.span)]))
            }
        })
    }

    /// Generate code for a match expression, returning a pointer to the phi value if any
    fn gen_match_expr(
        &mut self,
        module: ModId,
        matched: &Ast<TypeId>,
        arms: &[(TypeId, Ast<TypeId>)],
        span: Span,
    ) -> CompilerRes<Option<PointerValue<'ctx>>> {
        let mut has_phi = false;
        let mut all_arms_have_phi = true;
        for (_, expr) in arms {
            if let AstNode::PhiExpr(_) = expr.node {
                has_phi = true;
            } else {
                all_arms_have_phi = false;
            }
        }

        if has_phi && !all_arms_have_phi {
            return Err(Diagnostic::error()
                .with_message(
                    "Cannot use match statement as expression because not all arms have phi",
                )
                .with_labels(vec![Label::primary(self.file, span)
                    .with_message("Match statement used as expression here")]));
        }

        let after_bb = self
            .ctx
            .append_basic_block(self.current_fun.unwrap().0, "after_match");

        let phi_data = if has_phi {
            let ty = self.ast_type(module, &arms[0].1)?;
            let llvm_ty = Self::require_basictype(self.file, span, self.llvm_ty(span, ty)?)?;
            Some(PhiData {
                alloca: self.builder.build_alloca(llvm_ty, "match_phi"),
                break_bb: after_bb,
                phi_ty: ty,
            })
        } else {
            None
        };

        let old_phi_data = self.phi_data;
        self.phi_data = phi_data;

        let matched_ty = self.ast_type(module, matched)?;
        let matched_ty = self.spark.unwrap_alias(matched_ty);
        let matched_parts = if let TypeData::Enum { ref parts } = self.spark[matched_ty] {
            parts.clone()
        } else {
            return Err(Diagnostic::error()
                .with_message(format!(
                    "Cannot match against non-enum type {}",
                    self.spark.get_type_name(matched_ty)
                ))
                .with_labels(vec![Label::primary(self.file, matched.span)]));
        };

        let matched = self.gen_lval(module, matched)?;
        let discr = self
            .builder
            .build_struct_gep(matched, 0, "enum_match_discr")
            .unwrap();
        let discr = self
            .builder
            .build_load(discr, "enum_match_discr_load")
            .into_int_value();

        let start_bb = self.builder.get_insert_block().unwrap();

        let cases = arms
            .into_iter()
            .map(|(ty, expr)| {
                if let Some(idx) = matched_parts.iter().position(|part| *part == *ty) {
                    let arm_bb = self
                        .ctx
                        .append_basic_block(self.current_fun.unwrap().0, "matcharm_bb");
                    self.builder.position_at_end(arm_bb);
                    match self.gen_stmt(module, expr) {
                        Ok(_) => {
                            if !self.placed_terminator {
                                self.builder.build_unconditional_branch(after_bb);
                            }
                            Ok((self.ctx.i8_type().const_int(idx as u64, false), arm_bb))
                        }
                        Err(e) => Err(e),
                    }
                } else {
                    Err(Diagnostic::error()
                        .with_message(format!(
                            "Cannot match type {} that is not contained in matched enum type {}",
                            self.spark.get_type_name(*ty),
                            self.spark.get_type_name(matched_ty)
                        ))
                        .with_labels(vec![Label::primary(self.file, expr.span)]))
                }
            })
            .collect::<Result<Vec<_>, _>>()?;

        self.builder.position_at_end(start_bb);
        self.builder.build_switch(discr, after_bb, &cases);
        self.builder.position_at_end(after_bb);

        let phi_alloca = self.phi_data.map(|data| data.alloca);
        self.phi_data = old_phi_data;
        Ok(phi_alloca)
    }

    /// Generate code for a literal
    fn gen_literal(
        &mut self,
        module: ModId,
        literal: &Literal<TypeId>,
        span: Span,
    ) -> CompilerRes<BasicValueEnum<'ctx>> {
        Ok(match literal {
            Literal::Bool(b) => match b {
                true => self.ctx.bool_type().const_all_ones(),
                false => self.ctx.bool_type().const_zero(),
            }
            .into(),
            Literal::String(s) => {
                let glob = self
                    .builder
                    .build_global_string_ptr(s.as_str(), "const_str");
                glob.as_pointer_value().into()
            },
            Literal::Struct {
                ty,
                fields
            } => {
                    let typedata = ty.map(|ty| {
                        let ty = self.spark.unwrap_alias(ty);
                        self.spark[ty].clone()
                    });
                    let field_types = match typedata {
                        Some(TypeData::Struct{fields}) => fields,
                        None => fields.iter()
                            .map(|(name, expr)| match self.ast_type(module, expr) {
                                Ok(ty) => Ok((ty, name.clone())),
                                Err(e) => Err(e),
                            })
                            .collect::<Result<Vec<_>, _>>()?,
                        Some(_) => return Err(Diagnostic::error()
                            .with_message(format!(
                                    "Cannot create structure literal with non-struct type {}",
                                    self.spark.get_type_name(ty.unwrap())
                                )
                            )
                            .with_labels(vec![
                                Label::primary(self.file, span)
                                    .with_message("Structure literal encountered here")
                            ])
                        )
                    };
                    let ty = self.spark.new_type(TypeData::Struct{fields: field_types.clone()});

                    let llvm_ty = self.llvm_ty(span, ty)?.into_struct_type();
                    let struct_alloca = self.builder.build_alloca(llvm_ty, "struct_literal_alloca");
                    
                    for (name, fieldexpr) in fields {
                        if let Some(idx) = field_types.iter().position(|(_ty, fname)| fname == name) {
                            let field_ty = self.ast_type(module, fieldexpr)?;
                            if field_ty != field_types[idx].0 {
                                return Err(Diagnostic::error()
                                    .with_message(format!(
                                            "Assigning value of type {} to non-compatible field type {}",
                                            self.spark.get_type_name(field_ty),
                                            self.spark.get_type_name(field_types[idx].0)
                                        )
                                    )
                                    .with_labels(vec![
                                        Label::primary(self.file, fieldexpr.span)
                                            .with_message("Assignment to field here")
                                    ])
                                )
                            }

                            let fieldexpr_llvm = self.gen_expr(module, fieldexpr)?;
                            let structfield_ptr = self.builder.build_struct_gep(
                                struct_alloca,
                                idx as u32,
                                "struct_literal_field"
                            ).unwrap();

                            self.builder.build_store(structfield_ptr, fieldexpr_llvm);
                        } else {
                            return Err(Diagnostic::error()
                                .with_message(format!(
                                        "Assigning to field {} not contained in structure type {}",
                                        name,
                                        self.spark.get_type_name(ty)
                                    )
                                )
                                .with_labels(vec![
                                    Label::primary(self.file, fieldexpr.span)
                                        .with_message("Field assigned here")
                                ])
                            )
                        }
                    }
                    
                    self.builder.build_load(struct_alloca, "struct_literal_load")
                }
            Literal::Array(elems) => {
                if elems.len() == 0 {
                    return Err(Diagnostic::error()
                        .with_message("Cannot create array literal with zero elements")
                        .with_labels(vec![
                            Label::primary(self.file, span)
                        ])
                    )
                }

                let elem_ty = self.ast_type(module, &elems[0])?;
                for elem in elems.iter() {
                    let ty = self.ast_type(module, elem)?;
                    if ty != elem_ty {
                        return Err(Diagnostic::error()
                            .with_message("Creating array literal with mismatched element types")
                            .with_labels(vec![
                                Label::primary(self.file, elem.span)
                                    .with_message(format!(
                                        "This element has type {}",
                                        self.spark.get_type_name(ty)
                                    )),
                                Label::primary(self.file, elems[0].span)
                                    .with_message(format!(
                                        "First element has type {}",
                                        self.spark.get_type_name(elem_ty)
                                    ))
                            ])
                        )
                    } 
                }

                let llvm_elem_type = Self::require_basictype(self.file, elems[0].span, self.llvm_ty(elems[0].span, elem_ty)?)?;

                let array_alloca = self.builder.build_alloca(
                    llvm_elem_type.array_type(elems.len() as u32),
                    "array_literal_alloca"
                );

                for (i, elem) in elems.iter().enumerate() {
                    let elem = self.gen_expr(module, elem)?;
                    let elem_ptr = unsafe {
                        self.builder.build_in_bounds_gep(
                            array_alloca,
                            &[
                                self.ctx.i64_type().const_int(0, false),
                                self.ctx.i64_type().const_int(i as u64, false)
                            ],
                            "array_literal_gep"
                        )
                    };
                    self.builder.build_store(elem_ptr, elem);
                }

                self.builder.build_load(array_alloca, "array_literal_load")
            },
            Literal::Number(n) => {
                match n {
                    NumberLiteral::Integer(num, annot) => {
                        match annot {
                            Some(annot) => {
                                let n =
                                    match annot {
                                        NumberLiteralAnnotation::U8
                                        | NumberLiteralAnnotation::I8 => self.ctx.i8_type(),
                                        NumberLiteralAnnotation::U16
                                        | NumberLiteralAnnotation::I16 => self.ctx.i16_type(),
                                        NumberLiteralAnnotation::U32
                                        | NumberLiteralAnnotation::I32 => self.ctx.i32_type(),
                                        NumberLiteralAnnotation::U64
                                        | NumberLiteralAnnotation::I64 => self.ctx.i64_type(),
                                        NumberLiteralAnnotation::F32
                                        | NumberLiteralAnnotation::F64 => self.ctx.i64_type(),
                                    }
                                    .const_int(num.val, num.sign);

                                match annot {
                                    NumberLiteralAnnotation::F32 => {
                                        if num.sign {
                                            self.builder
                                                .build_signed_int_to_float(
                                                    n,
                                                    self.ctx.f32_type(),
                                                    "numliteral_cast",
                                                )
                                                .into()
                                        } else {
                                            self.builder
                                                .build_unsigned_int_to_float(
                                                    n,
                                                    self.ctx.f32_type(),
                                                    "numliteral_cast",
                                                )
                                                .into()
                                        }
                                    }
                                    NumberLiteralAnnotation::F64 => {
                                        if num.sign {
                                            self.builder
                                                .build_signed_int_to_float(
                                                    n,
                                                    self.ctx.f64_type(),
                                                    "numliteral_cast",
                                                )
                                                .into()
                                        } else {
                                            self.builder
                                                .build_unsigned_int_to_float(
                                                    n,
                                                    self.ctx.f64_type(),
                                                    "numliteral_cast",
                                                )
                                                .into()
                                        }
                                    }
                                    _ => n.into(),
                                }
                            }
                            None => self.ctx.i32_type().const_int(num.val, num.sign).into(),
                        }
                    }
                    NumberLiteral::Float(f, annot) => match annot {
                        Some(annot) => {
                            if let NumberLiteralAnnotation::F32 = annot {
                                self.ctx.f32_type().const_float(*f).into()
                            } else {
                                let f = self.ctx.f64_type().const_float(*f);

                                match annot {
                                    NumberLiteralAnnotation::U8 => self
                                        .builder
                                        .build_float_to_unsigned_int(
                                            f,
                                            self.ctx.i8_type(),
                                            "numberliteral_cast",
                                        )
                                        .into(),
                                    NumberLiteralAnnotation::U16 => self
                                        .builder
                                        .build_float_to_unsigned_int(
                                            f,
                                            self.ctx.i16_type(),
                                            "numberliteral_cast",
                                        )
                                        .into(),
                                    NumberLiteralAnnotation::U32 => self
                                        .builder
                                        .build_float_to_unsigned_int(
                                            f,
                                            self.ctx.i32_type(),
                                            "numberliteral_cast",
                                        )
                                        .into(),
                                    NumberLiteralAnnotation::U64 => self
                                        .builder
                                        .build_float_to_unsigned_int(
                                            f,
                                            self.ctx.i64_type(),
                                            "numberliteral_cast",
                                        )
                                        .into(),
                                    NumberLiteralAnnotation::I8 => self
                                        .builder
                                        .build_float_to_signed_int(
                                            f,
                                            self.ctx.i8_type(),
                                            "numberliteral_cast",
                                        )
                                        .into(),
                                    NumberLiteralAnnotation::I16 => self
                                        .builder
                                        .build_float_to_signed_int(
                                            f,
                                            self.ctx.i16_type(),
                                            "numberliteral_cast",
                                        )
                                        .into(),
                                    NumberLiteralAnnotation::I32 => self
                                        .builder
                                        .build_float_to_signed_int(
                                            f,
                                            self.ctx.i32_type(),
                                            "numberliteral_cast",
                                        )
                                        .into(),
                                    NumberLiteralAnnotation::I64 => self
                                        .builder
                                        .build_float_to_signed_int(
                                            f,
                                            self.ctx.i64_type(),
                                            "numberliteral_cast",
                                        )
                                        .into(),
                                    NumberLiteralAnnotation::F64 => f.into(),
                                    NumberLiteralAnnotation::F32 => unreachable!(),
                                }
                            }
                        }
                        None => self.ctx.f64_type().const_float(*f).into(),
                    },
                }
            }
            _ => unimplemented!(),
        })
    }

    /// Generate code for a single binary expression
    fn gen_bin_expr(
        &mut self,
        module: ModId,
        lhs: &Ast<TypeId>,
        op: Op,
        rhs: &Ast<TypeId>,
    ) -> CompilerRes<BasicValueEnum<'ctx>> {
        let lhs_ty = self.ast_type(module, lhs)?;
        let rhs_ty = self.ast_type(module, rhs)?;

        let llvm_lhs = self.gen_expr(module, lhs)?;
        let llvm_rhs = self.gen_expr(module, rhs)?;

        if lhs_ty == rhs_ty {
            match (op, &self.spark[lhs_ty]) {
                (Op::Star, TypeData::Integer { .. }) => {
                    return Ok(self
                        .builder
                        .build_int_mul(llvm_lhs.into_int_value(), llvm_rhs.into_int_value(), "imul")
                        .into())
                }
                (Op::Div, TypeData::Integer { signed: true, .. }) => {
                    return Ok(self
                        .builder
                        .build_int_signed_div(
                            llvm_lhs.into_int_value(),
                            llvm_rhs.into_int_value(),
                            "sidiv",
                        )
                        .into())
                }
                (Op::Div, TypeData::Integer { signed: false, .. }) => {
                    return Ok(self
                        .builder
                        .build_int_unsigned_div(
                            llvm_lhs.into_int_value(),
                            llvm_rhs.into_int_value(),
                            "uidiv",
                        )
                        .into())
                }
                (Op::Add, TypeData::Integer { .. }) => {
                    return Ok(self
                        .builder
                        .build_int_add(llvm_lhs.into_int_value(), llvm_rhs.into_int_value(), "iadd")
                        .into())
                }
                (Op::Sub, TypeData::Integer { .. }) => {
                    return Ok(self
                        .builder
                        .build_int_sub(llvm_lhs.into_int_value(), llvm_rhs.into_int_value(), "isub")
                        .into())
                }
                (Op::Mod, TypeData::Integer { signed: true, .. }) => {
                    return Ok(self
                        .builder
                        .build_int_signed_rem(
                            llvm_lhs.into_int_value(),
                            llvm_rhs.into_int_value(),
                            "simod",
                        )
                        .into())
                }
                (Op::Mod, TypeData::Integer { signed: false, .. }) => {
                    return Ok(self
                        .builder
                        .build_int_unsigned_rem(
                            llvm_lhs.into_int_value(),
                            llvm_rhs.into_int_value(),
                            "uimod",
                        )
                        .into())
                }

                (
                    Op::Eq | Op::Greater | Op::GreaterEq | Op::Less | Op::LessEq,
                    TypeData::Integer { signed, .. },
                ) => {
                    return Ok(self
                        .builder
                        .build_int_compare(
                            match (op, signed) {
                                (Op::Eq, _) => IntPredicate::EQ,
                                (Op::Greater, true) => IntPredicate::SGT,
                                (Op::Greater, false) => IntPredicate::UGT,
                                (Op::GreaterEq, true) => IntPredicate::SGE,
                                (Op::GreaterEq, false) => IntPredicate::UGE,
                                (Op::Less, true) => IntPredicate::SLT,
                                (Op::Less, false) => IntPredicate::ULT,
                                (Op::LessEq, true) => IntPredicate::SLE,
                                (Op::LessEq, false) => IntPredicate::ULE,
                                _ => unreachable!(),
                            },
                            llvm_lhs.into_int_value(),
                            llvm_rhs.into_int_value(),
                            "icmp",
                        )
                        .into())
                }

                (
                    Op::Eq | Op::Greater | Op::GreaterEq | Op::Less | Op::LessEq,
                    TypeData::Float { .. },
                ) => {
                    return Ok(self
                        .builder
                        .build_float_compare(
                            match op {
                                Op::Eq => FloatPredicate::OEQ,
                                Op::Greater => FloatPredicate::OGT,
                                Op::GreaterEq => FloatPredicate::OGE,
                                Op::Less => FloatPredicate::OLT,
                                Op::LessEq => FloatPredicate::OLE,
                                _ => unreachable!(),
                            },
                            llvm_lhs.into_float_value(),
                            llvm_rhs.into_float_value(),
                            "fcmp",
                        )
                        .into())
                }

                (Op::Star, TypeData::Float { .. }) => {
                    return Ok(self
                        .builder
                        .build_float_mul(
                            llvm_lhs.into_float_value(),
                            llvm_rhs.into_float_value(),
                            "fmul",
                        )
                        .into())
                }
                (Op::Div, TypeData::Float { .. }) => {
                    return Ok(self
                        .builder
                        .build_float_div(
                            llvm_lhs.into_float_value(),
                            llvm_rhs.into_float_value(),
                            "fdiv",
                        )
                        .into())
                }
                (Op::Add, TypeData::Float { .. }) => {
                    return Ok(self
                        .builder
                        .build_float_add(
                            llvm_lhs.into_float_value(),
                            llvm_rhs.into_float_value(),
                            "fadd",
                        )
                        .into())
                }
                (Op::Sub, TypeData::Float { .. }) => {
                    return Ok(self
                        .builder
                        .build_float_sub(
                            llvm_lhs.into_float_value(),
                            llvm_rhs.into_float_value(),
                            "fsub",
                        )
                        .into())
                }
                (Op::Mod, TypeData::Float { .. }) => {
                    return Ok(self
                        .builder
                        .build_float_rem(
                            llvm_lhs.into_float_value(),
                            llvm_rhs.into_float_value(),
                            "fmod",
                        )
                        .into())
                }
                _ => (),
            }
        }

        Ok(
            match (self.spark[lhs_ty].clone(), op, self.spark[rhs_ty].clone()) {
                (TypeData::Integer { .. }, Op::ShLeft, TypeData::Integer { .. }) => self
                    .builder
                    .build_left_shift(llvm_lhs.into_int_value(), llvm_rhs.into_int_value(), "ishl")
                    .into(),
                (TypeData::Integer { signed, .. }, Op::ShRight, TypeData::Integer { .. }) => self
                    .builder
                    .build_right_shift(
                        llvm_lhs.into_int_value(),
                        llvm_rhs.into_int_value(),
                        signed,
                        "ishr",
                    )
                    .into(),
                _ => {
                    return Err(Diagnostic::error()
                        .with_message(format!(
                            "Binary operator {} cannot be applied to the given types",
                            op
                        ))
                        .with_labels(vec![
                            Label::primary(self.file, lhs.span).with_message(format!(
                                "Left hand side is found to be of type {}",
                                self.spark.get_type_name(lhs_ty)
                            )),
                            Label::primary(self.file, rhs.span).with_message(format!(
                                "Right hand side is found to be of type {}",
                                self.spark.get_type_name(rhs_ty)
                            )),
                        ]))
                }
            },
        )
    }

    /// Generate an lvalue expression, returning a [PointerValue] to the lval
    fn gen_lval(
        &mut self,
        module: ModId,
        ast: &Ast<TypeId>,
    ) -> CompilerRes<PointerValue<'ctx>> {
        Ok(match &ast.node {
            AstNode::Access(path) => return self.gen_access(ast.span, path),
            AstNode::Block(block) => {
                if let Some(pv) = self.gen_block_ast(module, block)? {
                    pv
                } else {
                    return Err(Diagnostic::error()
                        .with_message("Cannot use block without phi statement as expression")
                        .with_labels(vec![Label::primary(self.file, ast.span)]));
                }
            },
            AstNode::Match { matched, cases } => {
                if let Some(pv) = self.gen_match_expr(module, matched, cases, ast.span)? {
                    pv
                } else {
                    return Err(Diagnostic::error()
                        .with_message(
                            "Cannot use match expression without phi nodes as an expression",
                        )
                        .with_labels(vec![Label::primary(self.file, ast.span)]));
                }
            }
            AstNode::IfExpr(if_expr) => {
                if let Some(pv) = self.gen_if_expr(module, if_expr)? {
                    pv
                } else {
                    return Err(Diagnostic::error()
                        .with_message("Cannot use if block with no phi nodes as expression")
                        .with_labels(vec![Label::primary(self.file, ast.span)]));
                }
            }
            AstNode::MemberAccess(object, field) => {
                self.gen_member(module, object, *field)?
            }
            _ => {
                let expr = self.gen_expr(module, ast)?;
                let alloca = self.builder.build_alloca(expr.get_type(), "lvalue_alloca");
                alloca
            }
        })
    }

    fn gen_block_ast(
        &mut self,
        module: ModId,
        block: &[Ast<TypeId>],
    ) -> CompilerRes<Option<PointerValue<'ctx>>> {
        let old_continue = self.continue_bb;
        let start_bb = self.builder.get_insert_block().unwrap();
        let body_bb = self
            .ctx
            .append_basic_block(self.current_fun.unwrap().0, "block");
        self.continue_bb = Some(body_bb);
        let after_bb = self
            .ctx
            .append_basic_block(self.current_fun.unwrap().0, "after");
        self.break_bb = Some(after_bb);

        //self.builder.position_at_end(start_bb);

        let pv = self.gen_body(module, block, body_bb, after_bb)?;
        self.builder.position_at_end(start_bb);
        self.builder.build_unconditional_branch(body_bb);

        self.builder.position_at_end(after_bb);
        self.continue_bb = old_continue;
        Ok(pv.map(|phi| phi.alloca))
    }

    /// Generate LLVM IR for a symbol access
    fn gen_access(
        &mut self,
        span: Span,
        path: &SymbolPath,
    ) -> CompilerRes<PointerValue<'ctx>> {
        let def = self.find_in_scope(span, path)?;
        Ok(match def {
            ScopeDef::Def(SparkDef::FunDef(_, fun)) => {
                let llvm_fun = self.llvm_funs[&fun];
                llvm_fun.as_global_value().as_pointer_value()
            }
            ScopeDef::Value(_, ptr) => ptr,
            _ => {
                return Err(Diagnostic::error()
                    .with_message(format!(
                        "Cannot use {} as an expression value",
                        match def {
                            ScopeDef::Def(SparkDef::ModDef(submod)) =>
                                format!("module '{}'", self.spark[submod].name),
                            ScopeDef::Def(SparkDef::TypeDef(_, ty)) =>
                                format!("type '{}'", self.spark.get_type_name(ty)),
                            ScopeDef::Value(..) => unreachable!(),
                            ScopeDef::Def(SparkDef::FunDef(..)) => unreachable!(),
                        }
                    ))
                    .with_labels(vec![Label::primary(self.file, span)]))
            }
        })
    }

    /// Generate code for a cast expression
    fn gen_cast(
        &mut self,
        module: ModId,
        to_ty: TypeId,
        rhs: &Ast<TypeId>,
    ) -> CompilerRes<BasicValueEnum<'ctx>> {
        let rhs_ty = self
            .ast_type(module, rhs)
            .map_err(|d| d.with_notes(vec!["In cast expression".to_owned()]))?;
        let to = self.spark[to_ty].clone();
        let from = self.spark[rhs_ty].clone();

        if self.spark.unwrap_alias(to_ty) == self.spark.unwrap_alias(rhs_ty) {
            return self.gen_expr(module, rhs)
        }

        //Generate an enum literal from a cast to an enum that contains the casted
        //type as a variant
        if let TypeData::Enum { parts } = &self.spark[self.spark.unwrap_alias(to_ty)] {
            let idx =
                parts.iter().enumerate().find_map(
                    |(idx, ty)| {
                        if *ty == rhs_ty {
                            Some(idx)
                        } else {
                            None
                        }
                    },
                );

            if let Some(idx) = idx {
                let enum_ty = Self::require_basictype(self.file, rhs.span, self.llvm_ty(rhs.span, to_ty)?)?;

                let enum_literal = self.builder.build_alloca(enum_ty, "enum_literal_alloca");

                let discrim = self
                    .builder
                    .build_struct_gep(enum_literal, 0, "enum_literal_get_discrim")
                    .unwrap();
                self.builder
                    .build_store(discrim, self.ctx.i8_type().const_int(idx as u64, false));
                
                if self.size_of_type(rhs_ty) != 0 {
                    let llvm_rhs = self.gen_expr(module, rhs)?;
                    let llvm_rhs_ty = Self::require_basictype(self.file, rhs.span, self.llvm_ty(rhs.span, rhs_ty)?)?;
                    let variant = self
                        .builder
                        .build_struct_gep(enum_literal, 1, "enum_literal_get_variant")
                        .unwrap();

                    let variant_ptr = self
                        .builder
                        .build_bitcast(
                            variant,
                            llvm_rhs_ty.ptr_type(AddressSpace::Generic),
                            "enum_variant_bc",
                        )
                        .into_pointer_value();

                    self.builder.build_store(variant_ptr, llvm_rhs);

                    let enum_literal_load = self.builder.build_load(enum_literal, "enum_lit_load");
                    return Ok(enum_literal_load.into())
                } else {
                    return Ok(self.builder.build_load(enum_literal, "enum_lit_load_no_variant"))
                }
            } else {
                return Err(Diagnostic::error()
                    .with_message(
                        "Attempting to cast to an enum type that does not contain castee type",
                    )
                    .with_labels(vec![Label::primary(self.file, rhs.span).with_message(format!(
                        "Attempted to cast type {} to enum type {}",
                        self.spark.get_type_name(rhs_ty),
                        self.spark.get_type_name(to_ty)
                    ))]));
            }
        }

        //Generate a bitcast to the desired type if casting from enum
        if let TypeData::Enum { parts } = &self.spark[self.spark.unwrap_alias(rhs_ty)] {
            if let Some(_idx) = parts.iter().position(|part| *part == to_ty) {
                let llvm_rhs = self.gen_lval(module, rhs)?;
                let llvm_to_ty = Self::require_basictype(self.file, rhs.span, self.llvm_ty(rhs.span, to_ty)?)?;

                let variant = self
                    .builder
                    .build_struct_gep(llvm_rhs, 1, "enum_variant_ptr")
                    .unwrap();

                let variant_bc = self
                    .builder
                    .build_bitcast(
                        variant,
                        llvm_to_ty.ptr_type(AddressSpace::Generic),
                        "enum_load_cast",
                    )
                    .into_pointer_value();
                if BasicTypeEnum::try_from(variant_bc.get_type().get_element_type()).unwrap()
                    != llvm_to_ty
                {
                    panic!("Casting to {:#?}", variant_bc.get_type());
                }

                return Ok(self.builder.build_load(variant_bc, "enum_data_load"));
            } else {
                return Err(Diagnostic::error()
                    .with_message(format!(
                        "Cannot cast enum type {} to type {}",
                        self.spark.get_type_name(self.spark.unwrap_alias(rhs_ty)),
                        self.spark.get_type_name(to_ty)
                    ))
                    .with_labels(vec![Label::primary(self.file, rhs.span)]));
            }
        }

        let llvm_rhs = self.gen_expr(module, rhs)?;

        Ok(match (from, to) {
            (
                TypeData::Integer {
                    width: from_width,
                    signed: _,
                },
                TypeData::Integer {
                    signed: to_sign,
                    width: to_width,
                },
            ) => {
                let llvm_to = self.llvm_int_ty(to_width);

                if let BasicValueEnum::IntValue(iv) = llvm_rhs {
                    if from_width == to_width {
                        iv.into()
                    } else if from_width < to_width && !to_sign {
                        self.builder
                            .build_int_z_extend(iv, llvm_to, "zext_upcast")
                            .into()
                    } else if from_width < to_width && to_sign {
                        self.builder
                            .build_int_s_extend(iv, llvm_to, "sext_upcast")
                            .into()
                    } else {
                        self.builder
                            .build_int_truncate(iv, llvm_to, "itrunc_downcast")
                            .into()
                    }
                } else {
                    println!("{:?}", llvm_rhs.get_type());
                    unreachable!()
                }
            }
            (TypeData::Integer { .. }, TypeData::Pointer(_)) => {
                let llvm_to = self.llvm_ty(rhs.span, to_ty)?.into_pointer_type();
                if let BasicValueEnum::IntValue(iv) = llvm_rhs {
                    self.builder
                        .build_int_to_ptr(iv, llvm_to, "int_to_ptr")
                        .into()
                } else {
                    unreachable!()
                }
            }
            (TypeData::Integer { signed, .. }, TypeData::Float { .. }) => {
                let llvm_to = self.llvm_ty(rhs.span, to_ty)?.into_float_type();
                if let BasicValueEnum::IntValue(iv) = llvm_rhs {
                    if signed {
                        self.builder
                            .build_signed_int_to_float(iv, llvm_to, "s_to_f")
                            .into()
                    } else {
                        self.builder
                            .build_unsigned_int_to_float(iv, llvm_to, "u_to_f")
                            .into()
                    }
                } else {
                    unreachable!()
                }
            }
            (TypeData::Float { .. }, TypeData::Integer { signed, width }) => {
                let llvm_to = self.llvm_int_ty(width);
                if let BasicValueEnum::FloatValue(fv) = llvm_rhs {
                    if signed {
                        self.builder
                            .build_float_to_signed_int(fv, llvm_to, "f_to_s")
                            .into()
                    } else {
                        self.builder
                            .build_float_to_unsigned_int(fv, llvm_to, "f_to_u")
                            .into()
                    }
                } else {
                    unreachable!()
                }
            }
            (TypeData::Pointer(..), TypeData::Pointer(..)) => {
                let llvm_to = self.llvm_ty(rhs.span, to_ty)?.into_pointer_type();
                if let BasicValueEnum::PointerValue(pv) = llvm_rhs {
                    self.builder
                        .build_pointer_cast(pv, llvm_to, "ptr_to_ptr")
                        .into()
                } else {
                    unreachable!()
                }
            }
            (TypeData::Pointer(..), TypeData::Integer { signed, width }) => {
                let llvm_to = self.llvm_int_ty(width);
                if let BasicValueEnum::PointerValue(pv) = llvm_rhs {
                    let int = self.builder.build_ptr_to_int(pv, llvm_to, "ptr_to_u");
                    if signed {
                        self.builder
                            .build_int_s_extend_or_bit_cast(int, llvm_to, "ptr_to_u_to_i")
                            .into()
                    } else {
                        int.into()
                    }
                } else {
                    unreachable!()
                }
            }
            _ => {
                return Err(Diagnostic::error()
                    .with_message(format!(
                        "Cannot cast value of type {} to {}",
                        self.spark.get_type_name(rhs_ty),
                        self.spark.get_type_name(to_ty)
                    ))
                    .with_labels(vec![Label::primary(self.file, rhs.span)]))
            }
        })
    }

    /// Generate code for a single if expression or statement
    fn gen_if_expr(
        &mut self,
        module: ModId,
        if_expr: &IfExpr<TypeId>,
    ) -> CompilerRes<Option<PointerValue<'ctx>>> {
        let start_bb = self.builder.get_insert_block().unwrap();

        let cond_ty = self.ast_type(module, &if_expr.cond)?;
        if let TypeData::Bool = &self.spark[cond_ty] {
            let cond = self.gen_expr(module, &if_expr.cond)?.into_int_value();
            let if_body_block = self
                .ctx
                .append_basic_block(self.current_fun.unwrap().0, "if_body");

            match &if_expr.else_expr {
                Some(else_expr) => {
                    let else_bb = self
                        .ctx
                        .append_basic_block(self.current_fun.unwrap().0, "else_bb");
                    let after_bb = self
                        .ctx
                        .append_basic_block(self.current_fun.unwrap().0, "after_bb");

                    let if_phi =
                        self.gen_body(module, &if_expr.body, if_body_block, after_bb)?;
                    let old_phi_data = self.phi_data;
                    self.phi_data = if_phi;

                    match else_expr {
                        ElseExpr::ElseIf(elif_expr) => {
                            self.builder.position_at_end(else_bb);
                            let else_phi = self.gen_if_expr(module, elif_expr)?;
                            if let (Some(if_pv), Some(else_pv)) = (if_phi, else_phi) {
                                let else_phi = self.builder.build_load(else_pv, "elif_phi");
                                self.builder.build_store(if_pv.alloca, else_phi);
                            }
                        }
                        ElseExpr::Else(else_body) => {
                            self.builder.position_at_end(start_bb);
                            self.gen_body_no_phi(module, else_body, else_bb, after_bb)?;
                        }
                    }
                    self.phi_data = old_phi_data;

                    self.builder.position_at_end(start_bb);
                    self.builder
                        .build_conditional_branch(cond, if_body_block, else_bb);
                    self.builder.position_at_end(after_bb);
                    Ok(if_phi.map(|phi| phi.alloca))
                }
                None => {
                    let after_bb = self
                        .ctx
                        .append_basic_block(self.current_fun.unwrap().0, "after_if");
                    let if_phi =
                        self.gen_body(module, &if_expr.body, if_body_block, after_bb)?;
                    self.builder.position_at_end(start_bb);
                    self.builder
                        .build_conditional_branch(cond, if_body_block, after_bb);
                    self.builder.position_at_end(after_bb);
                    Ok(if_phi.map(|phi| phi.alloca))
                }
            }
        } else {
            return Err(Diagnostic::error()
                .with_message(format!(
                    "Using value of type {} as boolean condition for if expression",
                    self.spark.get_type_name(cond_ty)
                ))
                .with_labels(vec![
                    Label::primary(self.file, if_expr.cond.span).with_message("Non-boolean value here")
                ]));
        }
    }

    /// Generate code for a single member access
    fn gen_member(
        &mut self,
        module: ModId,
        object: &Ast<TypeId>,
        field: Symbol,
    ) -> CompilerRes<PointerValue<'ctx>> {
        let obj_ty = self.ast_type(module, object)?;
        let obj_ty = self.spark.unwrap_alias(obj_ty);
        if let TypeData::Struct { ref fields } = self.spark[obj_ty] {
            let fields = fields.clone();
            let struct_pv = self.gen_lval(module, object)?;

            for (i, (_, name)) in fields.iter().enumerate() {
                if *name == field {
                    return Ok(self
                        .builder
                        .build_struct_gep(struct_pv, i as u32, "struct_field_access")
                        .unwrap());
                }
            }
            Err(Diagnostic::error()
                .with_message(format!(
                    "Structure type {} has no field named {}",
                    self.spark.get_type_name(obj_ty),
                    field
                ))
                .with_labels(vec![Label::primary(self.file, object.span).with_message(
                    format!(
                        "Expression of structure type {} encountered here",
                        self.spark.get_type_name(obj_ty)
                    ),
                )]))
        } else {
            Err(Diagnostic::error()
                .with_message(format!(
                    "Cannot access field {} of non-struct type {}",
                    field,
                    self.spark.get_type_name(obj_ty)
                ))
                .with_labels(vec![Label::primary(self.file, object.span).with_message(
                    format!(
                        "Expression of type {} encountered here",
                        self.spark.get_type_name(obj_ty)
                    ),
                )]))
        }
    }

    /// Generate code for a single function call and return the return value of the function or
    /// `None` if the function called returns the unit type
    fn gen_call(
        &mut self,
        module: ModId,
        called: &Ast<TypeId>,
        args: &[Ast<TypeId>],
    ) -> CompilerRes<Option<BasicValueEnum<'ctx>>> {
        let called_ty = self.ast_type(module, called)?;
        if let TypeData::Function(f) = &self.spark[called_ty] {
            let f = f.clone();
            if f.args.len() != args.len() {
                return Err(Diagnostic::error()
                    .with_message("Passing invalid number of arguments to function")
                    .with_labels(vec![Label::primary(self.file, called.span).with_message(
                        format!("Expecting {} arguments, found {}", f.args.len(), args.len()),
                    )]));
            }

            let passed_types = args
                .iter()
                .map(|arg| match self.ast_type(module, arg) {
                    Ok(ty) => Ok((arg.span, ty)),
                    Err(e) => Err(e),
                })
                .collect::<Result<Vec<_>, _>>()?;

            for (expecting, (passed_span, passed_ty)) in f.args.iter().copied().zip(passed_types) {
                let expecting_ty = self.spark.unwrap_alias(expecting);
                let passed_ty = self.spark.unwrap_alias(passed_ty);
                if expecting_ty != passed_ty {
                    return Err(Diagnostic::error()
                        .with_message(format!(
                            "Passing invalid argument type '{}', expecting '{}'",
                            self.spark.get_type_name(passed_ty),
                            self.spark.get_type_name(expecting_ty)
                        ))
                        .with_labels(vec![Label::primary(self.file, passed_span)]));
                }
            }
            let called = self.gen_expr(module, called)?;
            match called {
                BasicValueEnum::PointerValue(pv) => match CallableValue::try_from(pv) {
                    Ok(callable) => {
                        let args = args
                            .iter()
                            .map(|arg| self.gen_expr(module, arg).map(|v| v.into()))
                            .collect::<Result<Vec<_>, _>>()?;
                        return Ok(self
                            .builder
                            .build_call(callable, &args, "fn_call")
                            .try_as_basic_value()
                            .left());
                    }
                    _ => (),
                },
                _ => (),
            }
        }

        Err(Diagnostic::error()
            .with_message("Cannot call a value of non-function type")
            .with_labels(vec![Label::primary(self.file, called.span).with_message(
                format!(
                    "Value of type {} found here",
                    self.spark.get_type_name(called_ty)
                ),
            )]))
    }
    
    /// Generate a body, creating a phi alloca automatically
    fn gen_body(
        &mut self,
        module: ModId,
        body: &[Ast<TypeId>],
        to_bb: BasicBlock<'ctx>,
        after_bb: BasicBlock<'ctx>
    ) -> CompilerRes<Option<PhiData<'ctx>>> {
        let phi_data = match Self::phi_node(self.file, body) {
            Err(_) => None,
            Ok(phi_node) => {
                let ty = self.ast_type(module, phi_node).unwrap();
                if let Ok(llvm_ty) = BasicTypeEnum::try_from(self.llvm_ty(phi_node.span, ty)?) {
                    let phi_alloca = self.builder.build_alloca(llvm_ty, "phi_alloca");

                    Some(PhiData {
                        break_bb: after_bb,
                        phi_ty: ty,
                        alloca: phi_alloca,
                    })
                } else {
                    None
                }
            }
        };
        let old_phi_data = self.phi_data;
        self.phi_data = phi_data;

        self.gen_body_no_phi(
            module,
            body,
            to_bb,
            after_bb,
        )?;
 
        self.phi_data = old_phi_data;
        Ok(phi_data)
    }

    /// Generate LLVM IR for a block of statements
    fn gen_body_no_phi(
        &mut self,
        module: ModId,
        body: &[Ast<TypeId>],
        to_bb: BasicBlock<'ctx>,
        after_bb: BasicBlock<'ctx>,
    ) -> CompilerRes<()> {
        self.builder.position_at_end(to_bb);

        self.current_scope.push_layer();

        for stmt in body.iter() {
            if let Err(e) = self.gen_stmt(module, stmt) {
                self.current_scope.pop_layer();
                self.builder.position_at_end(after_bb);
                return Err(e);
            }
            if self.placed_terminator {
                break;
            }
        }

        self.current_scope.pop_layer();
        if !self.placed_terminator {
            self.builder.build_unconditional_branch(after_bb);
        } else {
            self.placed_terminator = false;
        }
        self.builder.position_at_end(after_bb);

        Ok(())
    }

    /// Generate an LLVM integer type to match an IR integer type
    fn llvm_int_ty(&self, width: IntegerWidth) -> IntType<'ctx> {
        match width {
            IntegerWidth::Eight => self.ctx.i8_type(),
            IntegerWidth::Sixteen => self.ctx.i16_type(),
            IntegerWidth::ThirtyTwo => self.ctx.i32_type(),
            IntegerWidth::SixtyFour => self.ctx.i64_type(),
        }
    }

    /// Get the type of an AST expression
    fn ast_type(
        &mut self,
        module: ModId,
        ast: &Ast<TypeId>,
    ) -> CompilerRes<TypeId> {
        Ok(match &ast.node {
            AstNode::Literal(Literal::Struct {
                ty,
                fields
            }) => match ty {
                    Some(ty) => *ty,
                    None => {
                        let fields = fields.iter()
                            .map(|(name, field)| match self.ast_type(module, field) {
                                Ok(ty) => Ok((ty, name.clone())),
                                Err(e) => Err(e)
                            })
                            .collect::<Result<Vec<_>, _>>()?;
                        self.spark.new_type(TypeData::Struct {fields})
                    }
                }
            AstNode::Literal(Literal::Unit) => SparkCtx::UNIT,
            AstNode::Literal(Literal::Number(num)) => match num.annotation() {
                Some(ann) => match ann {
                    NumberLiteralAnnotation::I8 => SparkCtx::I8,
                    NumberLiteralAnnotation::I16 => SparkCtx::I16,
                    NumberLiteralAnnotation::I32 => SparkCtx::I32,
                    NumberLiteralAnnotation::I64 => SparkCtx::I64,
                    NumberLiteralAnnotation::U8 => SparkCtx::U8,
                    NumberLiteralAnnotation::U16 => SparkCtx::U16,
                    NumberLiteralAnnotation::U32 => SparkCtx::U32,
                    NumberLiteralAnnotation::U64 => SparkCtx::U64,
                    NumberLiteralAnnotation::F32 => SparkCtx::F32,
                    NumberLiteralAnnotation::F64 => SparkCtx::F64,
                },
                None => {
                    if let NumberLiteral::Float(..) = num {
                        SparkCtx::F64
                    } else {
                        SparkCtx::I32
                    }
                }
            },
            AstNode::Literal(Literal::String(_)) => {
                self.spark.new_type(TypeData::Pointer(SparkCtx::U8))
            }
            AstNode::Literal(Literal::Bool(_)) => SparkCtx::BOOL,
            AstNode::Literal(Literal::Array(parts)) => {
                let first_type = self.ast_type(module, parts.first().ok_or_else(||
                    Diagnostic::error()
                        .with_message("Failed to infer type of array literal because there are no elements")
                        .with_labels(vec![Label::primary(self.file, ast.span)])
                )?)?;
                self.spark.new_type(TypeData::Array {
                    element: first_type,
                    len: parts.len() as u64,
                })
            }
            AstNode::CastExpr(ty, ..) => *ty,
            AstNode::FunCall(called, ..) => {
                let called_ty = self.ast_type(module, called)?;
                if let TypeData::Function(f_ty) = &self.spark[called_ty] {
                    f_ty.return_ty
                } else {
                    return Err(Diagnostic::error()
                        .with_message(format!(
                            "Attempting to call a value of type '{}' as a function",
                            self.spark.get_type_name(called_ty)
                        ))
                        .with_labels(vec![Label::primary(self.file, called.span).with_message(
                            format!(
                                "Called value of type '{}' here",
                                self.spark.get_type_name(called_ty)
                            ),
                        )]));
                }
            }
            AstNode::Access(path) => {
                let def = self.find_in_scope(ast.span, path)?;

                match def {
                    ScopeDef::Def(SparkDef::FunDef(_, f)) => self
                        .spark
                        .new_type(TypeData::Function(self.spark[f].ty.clone())),
                    ScopeDef::Value(ty, _) => ty,
                    ScopeDef::Def(SparkDef::TypeDef(_file, ty)) if self.size_of_type(ty) == 0 => ty,
                    _ => {
                        return Err(Diagnostic::error()
                            .with_message("Cannot infer type of definition")
                            .with_labels(vec![Label::primary(self.file, ast.span)]))
                    }
                }
            }
            AstNode::MemberAccess(lhs, name) => {
                let lhs_ty = self.ast_type(module, lhs)?;
                let lhs_ty = self.spark.unwrap_alias(lhs_ty);
                if let TypeData::Struct { fields } = &self.spark[lhs_ty] {
                    fields.iter().find_map(|(ty, field_name)| if name == field_name {
                        Some(*ty)
                    } else {
                        None
                    }).ok_or_else(|| Diagnostic::error()
                        .with_message(format!(
                                "Attempting to index field '{}' of type '{}' but no such field exists",
                                name,
                                self.spark.get_type_name(lhs_ty)
                            )
                        )
                        .with_labels(vec![
                            Label::primary(self.file, lhs.span)
                                .with_message(format!("This expression is found to be of type '{}'", self.spark.get_type_name(lhs_ty)))
                        ])
                        
                    )?
                } else {
                    return Err(Diagnostic::error()
                        .with_message(format!(
                            "Attempting to access field {} of non-struct type '{}'",
                            name,
                            self.spark.get_type_name(lhs_ty)
                        ))
                        .with_labels(vec![Label::primary(self.file, lhs.span).with_message(format!(
                            "this expression is found to be of type '{}'",
                            self.spark.get_type_name(lhs_ty)
                        ))]));
                }
            }
            AstNode::Index { object, index: _ } => {
                let object_ty = self.ast_type(module, object)?;
                if let TypeData::Array { element, len: _ } = self.spark[object_ty] {
                    element
                } else {
                    return Err(Diagnostic::error()
                        .with_message(format!(
                            "Attempting to index into a value of type '{}'",
                            self.spark.get_type_name(object_ty)
                        ))
                        .with_labels(vec![Label::primary(self.file, object.span).with_message(
                            format!(
                                "This expression is found to be of type '{}'",
                                self.spark.get_type_name(object_ty)
                            ),
                        )]));
                }
            }
            AstNode::BinExpr(
                _,
                Op::Greater | Op::GreaterEq | Op::Less | Op::LessEq | Op::Eq,
                _,
            ) => SparkCtx::BOOL,
            AstNode::BinExpr(lhs, ..) => self.ast_type(module, lhs)?,
            AstNode::UnaryExpr(op, rhs) => {
                let rhs_ty = self.ast_type(module, rhs)?;
                match op {
                    Op::Star => {
                        if let TypeData::Pointer(pointee) = self.spark[rhs_ty] {
                            pointee
                        } else {
                            return Err(Diagnostic::error()
                                .with_message(
                                    "Attempting to dereference expression of non-pointer type",
                                )
                                .with_labels(vec![Label::primary(self.file, ast.span).with_message(
                                    format!(
                                        "This expression is found to be of type '{}'",
                                        self.spark.get_type_name(rhs_ty)
                                    ),
                                )]));
                        }
                    }
                    Op::AND => self.spark.new_type(TypeData::Pointer(rhs_ty)),
                    _ => {
                        return Err(Diagnostic::error()
                            .with_message(format!("Unsupported unary operator '{}' used", op))
                            .with_labels(vec![Label::primary(self.file, ast.span)]))
                    }
                }
            }
            AstNode::IfExpr(if_expr) => {
                let phi_node = Self::phi_node(self.file, &if_expr.body).map_err(|e| {
                    e.with_labels(vec![
                        Label::secondary(self.file, ast.span).with_message("In if body here")
                    ])
                })?;
                let phi_ty = self.ast_type(module, phi_node)?;
                phi_ty
            }

            AstNode::VarDeclaration { ty: Some(ty), .. } => *ty,
            AstNode::PhiExpr(phid) => self.ast_type(module, phid)?,
            AstNode::Return(..)
            | AstNode::Break
            | AstNode::Continue
            | AstNode::VarDeclaration { .. }
            | AstNode::Assignment { .. } => {
                return Err(Diagnostic::error()
                    .with_message("Cannot find type of statement")
                    .with_labels(vec![Label::primary(self.file, ast.span)]))
            }
            AstNode::Block(body) => {
                let phi_node = Self::phi_node(self.file, &body).map_err(|e| {
                    e.with_labels(vec![
                        Label::secondary(self.file, ast.span).with_message("In loop body here")
                    ])
                })?;
                self.ast_type(module, phi_node)?
            }
            AstNode::Match { matched: _, cases } => {
                let case_1 = cases.first().ok_or_else(|| {
                    Diagnostic::error()
                        .with_message("Failed to infer type of match expression")
                        .with_labels(vec![Label::primary(self.file, ast.span)])
                })?;
                self.ast_type(module, &case_1.1)?
            }
        })
    }

    /// Get the phi node from a block of AST nodes
    fn phi_node(file: FileId, body: &[Ast<TypeId>]) -> CompilerRes<&Ast<TypeId>> {
        body.iter()
            .find_map(|stmt| {
                if let AstNode::PhiExpr(_) = &stmt.node {
                    Some(stmt)
                } else {
                    None
                }
            })
            .ok_or_else(|| {
                Diagnostic::error()
                    .with_message("Failed to locate phi node in block of statements")
                    .with_labels(if let Some(first) = body.first() {
                        vec![Label::primary(file, first.span)
                            .with_message("First expression of block here")]
                    } else {
                        vec![]
                    })
            })
    }
}
