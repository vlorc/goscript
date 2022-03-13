use crate::ffi::*;
use crate::non_async_result;
use goscript_vm::instruction::ValueType;
use goscript_vm::metadata::GosMetadata;
use goscript_vm::objects::MetadataObjs;
use goscript_vm::value::{GosValue, IfaceUnderlying, PointerObj, RtMultiValResult, UserData};
use std::any::Any;
use std::cell::RefCell;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;

macro_rules! param_as_std_val {
    ($param:expr) => {{
        let ud = $param.as_pointer().as_user_data();
        ud.as_any().downcast_ref::<StdValue>().unwrap()
    }};
}

enum GosKind {
    Invalid = 0,
    Bool,
    Int,
    Int8,
    Int16,
    Int32,
    Int64,
    Uint,
    Uint8,
    Uint16,
    Uint32,
    Uint64,
    _Uintptr, // do not support for now
    Float32,
    Float64,
    Complex64,
    Complex128,
    Array,
    Chan,
    Func,
    Interface,
    Map,
    Ptr,
    Slice,
    String,
    Struct,
    UnsafePointer,
}

pub struct Reflect {}

impl Ffi for Reflect {
    fn call(
        &self,
        ctx: &FfiCallCtx,
        params: Vec<GosValue>,
    ) -> Pin<Box<dyn Future<Output = RtMultiValResult> + '_>> {
        match ctx.func_name {
            "value_of" => {
                let p = PointerObj::UserData(Rc::new(StdValue::value_of(&params[0])));
                non_async_result![GosValue::new_pointer(p)]
            }
            "type_of" => {
                let v = param_as_std_val!(params[0]);
                let (t, k) = StdType::type_of(&v.val, ctx);
                non_async_result![t, k]
            }
            "bool_val" => {
                let v = param_as_std_val!(params[0]);
                let (b, err) = v.bool_val();
                non_async_result![b, err]
            }
            "int_val" => {
                let v = param_as_std_val!(params[0]);
                let (i, err) = v.int_val();
                non_async_result![i, err]
            }
            "uint_val" => {
                let v = param_as_std_val!(params[0]);
                let (i, err) = v.uint_val();
                non_async_result![i, err]
            }
            "float_val" => {
                let v = param_as_std_val!(params[0]);
                let (i, err) = v.float_val();
                non_async_result![i, err]
            }
            "bytes_val" => {
                let v = param_as_std_val!(params[0]);
                let (i, err) = v.bytes_val();
                non_async_result![i, err]
            }
            _ => unreachable!(),
        }
    }
}

impl Reflect {
    pub fn new(_v: Vec<GosValue>) -> FfiCtorResult<Rc<RefCell<dyn Ffi>>> {
        Ok(Rc::new(RefCell::new(Reflect {})))
    }
}

#[derive(Clone, Debug)]
struct StdValue {
    val: GosValue,
}

impl UserData for StdValue {
    fn as_any(&self) -> &dyn Any {
        self
    }
}

impl StdValue {
    fn new(v: GosValue) -> StdValue {
        StdValue { val: v }
    }

    fn value_of(v: &GosValue) -> StdValue {
        let iface = v.as_interface().borrow();
        let v = match &iface.underlying() {
            IfaceUnderlying::Gos(v, _) => v.clone(),
            // todo: should we return something else?
            IfaceUnderlying::Ffi(_) => GosValue::Nil(iface.meta),
            IfaceUnderlying::None => GosValue::Nil(iface.meta),
        };
        StdValue::new(v)
    }

    fn bool_val(&self) -> (GosValue, GosValue) {
        (GosValue::new_nil(), GosValue::new_nil())
    }

    fn int_val(&self) -> (GosValue, GosValue) {
        (GosValue::Int(888), GosValue::new_str("".to_string()))
    }

    fn uint_val(&self) -> (GosValue, GosValue) {
        (GosValue::new_nil(), GosValue::new_nil())
    }

    fn float_val(&self) -> (GosValue, GosValue) {
        (GosValue::new_nil(), GosValue::new_nil())
    }

    fn bytes_val(&self) -> (GosValue, GosValue) {
        (GosValue::new_nil(), GosValue::new_nil())
    }
}

#[derive(Clone, Debug)]
struct StdType {
    meta: GosMetadata,
    mobjs: *const MetadataObjs,
}

impl UserData for StdType {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn eq(&self, other: &dyn UserData) -> bool {
        match other.as_any().downcast_ref::<StdType>() {
            Some(other_type) => {
                let objs = unsafe { &*self.mobjs };
                self.meta.identical(&other_type.meta, objs)
            }
            None => false,
        }
    }
}

impl StdType {
    fn new(m: GosMetadata, objs: &MetadataObjs) -> StdType {
        StdType {
            meta: m,
            mobjs: objs,
        }
    }

    fn type_of(val: &GosValue, ctx: &FfiCallCtx) -> (GosValue, GosValue) {
        let m = val.get_meta(ctx.vm_objs, ctx.stack);
        let typ = StdType::new(m, &ctx.vm_objs.metas);
        let kind = match m
            .get_underlying(&ctx.vm_objs.metas)
            .get_value_type(&ctx.vm_objs.metas)
        {
            ValueType::Bool => GosKind::Bool,
            ValueType::Int => GosKind::Int,
            ValueType::Int8 => GosKind::Int8,
            ValueType::Int16 => GosKind::Int16,
            ValueType::Int32 => GosKind::Int32,
            ValueType::Int64 => GosKind::Int64,
            ValueType::Uint => GosKind::Uint,
            ValueType::Uint8 => GosKind::Uint8,
            ValueType::Uint16 => GosKind::Uint16,
            ValueType::Uint32 => GosKind::Uint32,
            ValueType::Uint64 => GosKind::Uint64,
            ValueType::Float32 => GosKind::Float32,
            ValueType::Float64 => GosKind::Float64,
            ValueType::Complex64 => GosKind::Complex64,
            ValueType::Complex128 => GosKind::Complex128,
            ValueType::Array => GosKind::Array,
            ValueType::Channel => GosKind::Chan,
            ValueType::Closure => GosKind::Func,
            ValueType::Interface => GosKind::Interface,
            ValueType::Map => GosKind::Map,
            ValueType::Pointer => {
                let ptr: &PointerObj = &*val.as_pointer();
                match ptr {
                    PointerObj::UserData(_) => GosKind::UnsafePointer,
                    _ => GosKind::Ptr,
                }
            }
            ValueType::Slice => GosKind::Slice,
            ValueType::Str => GosKind::String,
            ValueType::Struct => GosKind::Struct,
            _ => GosKind::Invalid,
        };
        (
            GosValue::new_pointer(PointerObj::UserData(Rc::new(typ))),
            GosValue::Uint(kind as usize),
        )
    }
}