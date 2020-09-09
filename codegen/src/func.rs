#![allow(dead_code)]

use std::convert::TryFrom;

use goscript_vm::instruction::*;
use goscript_vm::objects::{EntIndex, FunctionVal};
use goscript_vm::value::*;

use goscript_parser::ast::*;
use goscript_parser::objects::Objects as AstObjects;

#[derive(Clone, Debug)]
pub struct IndexSelInfo {
    pub index: OpIndex,
    pub t1: ValueType,
    pub t2: ValueType,
    pub is_index: bool,
}

impl IndexSelInfo {
    pub fn new(index: OpIndex, t1: ValueType, t2: ValueType, is_index: bool) -> IndexSelInfo {
        IndexSelInfo {
            index: index,
            t1: t1,
            t2: t2,
            is_index: is_index,
        }
    }
}

/// LeftHandSide represents the left hand side of an assign stmt
/// Primitive stores index of lhs variable
/// IndexSelExpr stores the info of index or selection lhs
/// Deref stores the index of lhs on the stack
#[derive(Clone, Debug)]
pub enum LeftHandSide {
    Primitive(EntIndex),
    IndexSelExpr(IndexSelInfo),
    Deref(OpIndex),
}

pub trait FuncGen {
    fn add_params<'e>(&mut self, fl: &FieldList, o: &AstObjects) -> Result<usize, ()>;

    fn emit_load(&mut self, index: EntIndex, typ: ValueType);

    fn emit_store(
        &mut self,
        lhs: &LeftHandSide,
        rhs_index: OpIndex,
        op: Option<Opcode>,
        typ: ValueType,
    );

    fn emit_import(&mut self, index: OpIndex);

    fn emit_pop(&mut self, typ: ValueType);

    fn emit_load_field(&mut self, typ: ValueType, sel_type: ValueType);

    fn emit_load_field_imm(&mut self, imm: OpIndex, typ: ValueType);

    fn emit_load_index(&mut self, typ: ValueType, sel_type: ValueType);

    fn emit_load_index_imm(&mut self, imm: OpIndex, typ: ValueType);

    fn emit_return(&mut self);

    fn emit_return_init_pkg(&mut self, index: OpIndex);

    fn emit_pre_call(&mut self);

    fn emit_call(&mut self, has_ellipsis: bool);

    fn emit_new(&mut self, typ: ValueType);

    fn emit_range(&mut self);
}

impl FuncGen for FunctionVal {
    fn add_params<'e>(&mut self, fl: &FieldList, o: &AstObjects) -> Result<usize, ()> {
        let re = fl
            .list
            .iter()
            .map(|f| {
                let names = &o.fields[*f].names;
                if names.len() == 0 {
                    self.add_local(None, None);
                    1
                } else {
                    names
                        .iter()
                        .map(|n| {
                            let ident = &o.idents[*n];
                            self.add_local(ident.entity.clone().into_key(), None);
                        })
                        .count()
                }
            })
            .sum();
        Ok(re)
    }

    fn emit_load(&mut self, index: EntIndex, typ: ValueType) {
        match index {
            EntIndex::Const(i) => match self.const_val(i).clone() {
                //GosValue::Nil => self.emit_code(Opcode::PUSH_NIL),
                GosValue::Bool(b) if b => self.emit_code(Opcode::PUSH_TRUE),
                GosValue::Bool(b) if !b => self.emit_code(Opcode::PUSH_FALSE),
                GosValue::Int(i) if OpIndex::try_from(i).ok().is_some() => {
                    let imm: OpIndex = OpIndex::try_from(i).unwrap();
                    self.emit_inst(Opcode::PUSH_IMM, Some(typ), None, None, Some(imm));
                }
                _ => {
                    self.emit_inst(Opcode::PUSH_CONST, Some(typ), None, None, Some(i));
                }
            },
            EntIndex::LocalVar(i) => {
                self.emit_inst(Opcode::LOAD_LOCAL, Some(typ), None, None, Some(i))
            }
            EntIndex::UpValue(i) => {
                self.emit_inst(Opcode::LOAD_UPVALUE, Some(typ), None, None, Some(i))
            }
            EntIndex::PackageMember(i) => {
                self.emit_inst(Opcode::LOAD_THIS_PKG_FIELD, Some(typ), None, None, Some(i))
            }
            EntIndex::BuiltIn(op) => self.emit_code(op),
            EntIndex::Blank => unreachable!(),
        }
    }

    fn emit_store(
        &mut self,
        lhs: &LeftHandSide,
        rhs_index: OpIndex,
        op: Option<Opcode>,
        typ: ValueType,
    ) {
        if let LeftHandSide::Primitive(index) = lhs {
            if EntIndex::Blank == *index {
                return;
            }
        }

        let (code, i, t1, t2) = match lhs {
            LeftHandSide::Primitive(index) => match index {
                EntIndex::Const(_) => unreachable!(),
                EntIndex::LocalVar(i) => (Opcode::STORE_LOCAL, i, None, None),
                EntIndex::UpValue(i) => (Opcode::STORE_UPVALUE, i, None, None),
                EntIndex::PackageMember(i) => (
                    Opcode::STORE_THIS_PKG_FIELD,
                    i,
                    Some(ValueType::Package),
                    None,
                ),
                EntIndex::BuiltIn(_) => unreachable!(),
                EntIndex::Blank => unreachable!(),
            },
            LeftHandSide::IndexSelExpr(info) => {
                let op = if info.is_index {
                    Opcode::STORE_INDEX
                } else {
                    Opcode::STORE_FIELD
                };
                (op, &info.index, Some(info.t1), Some(info.t2))
            }
            LeftHandSide::Deref(i) => (Opcode::STORE_DEREF, i, None, None),
        };

        let mut inst = Instruction::new(code, Some(typ), t1, t2, None);
        assert!(rhs_index == -1 || op.is_none());
        let imm0 = op.map_or(rhs_index, |x| Instruction::code2index(x));
        inst.set_imm2(imm0, *i);
        self.code.push(inst);
    }

    fn emit_import(&mut self, index: OpIndex) {
        self.emit_inst(Opcode::IMPORT, None, None, None, Some(index));
        let mut cd = vec![
            Instruction::new(Opcode::PUSH_IMM, None, None, None, Some(0)),
            Instruction::new(
                Opcode::LOAD_FIELD,
                Some(ValueType::Package),
                Some(ValueType::Int),
                None,
                None,
            ),
            Instruction::new(Opcode::PRE_CALL, Some(ValueType::Closure), None, None, None),
            Instruction::new(Opcode::CALL, None, None, None, None),
        ];
        let offset = cd.len() as OpIndex;
        self.emit_inst(Opcode::JUMP_IF_NOT, None, None, None, Some(offset));
        self.code.append(&mut cd);
    }

    fn emit_pop(&mut self, typ: ValueType) {
        self.emit_inst(Opcode::POP, Some(typ), None, None, None);
    }

    fn emit_load_field(&mut self, typ: ValueType, sel_type: ValueType) {
        self.emit_inst(Opcode::LOAD_FIELD, Some(typ), Some(sel_type), None, None);
    }

    fn emit_load_field_imm(&mut self, imm: OpIndex, typ: ValueType) {
        self.emit_inst(Opcode::LOAD_FIELD_IMM, Some(typ), None, None, Some(imm));
    }

    fn emit_load_index(&mut self, typ: ValueType, index_type: ValueType) {
        self.emit_inst(Opcode::LOAD_INDEX, Some(typ), Some(index_type), None, None);
    }

    fn emit_load_index_imm(&mut self, imm: OpIndex, typ: ValueType) {
        self.emit_inst(Opcode::LOAD_INDEX_IMM, Some(typ), None, None, Some(imm));
    }

    fn emit_return(&mut self) {
        self.emit_inst(Opcode::RETURN, None, None, None, None);
    }

    fn emit_return_init_pkg(&mut self, index: OpIndex) {
        self.emit_inst(Opcode::RETURN_INIT_PKG, None, None, None, Some(index));
    }

    fn emit_pre_call(&mut self) {
        self.emit_inst(Opcode::PRE_CALL, None, None, None, None);
    }

    fn emit_call(&mut self, has_ellipsis: bool) {
        let op = if has_ellipsis {
            Opcode::CALL_ELLIPSIS
        } else {
            Opcode::CALL
        };
        self.emit_inst(op, None, None, None, None);
    }

    fn emit_new(&mut self, typ: ValueType) {
        self.emit_inst(Opcode::NEW, Some(typ), None, None, None);
    }

    fn emit_range(&mut self) {
        self.emit_inst(Opcode::RANGE, None, None, None, None);
    }
}
