pub mod builder;
pub mod const_eval;
pub mod dce;
pub mod elide_zst;
pub mod infer;
pub mod inline;
pub mod lang;
pub mod layout;
pub mod mono;

use crate::ast::{Attribute, BinOp, BuiltinType, Span, UnOp};
use crate::common::{
    impl_allocatable, Allocatable, AluminaError, ArenaAllocatable, CodeErrorKind, HashSet,
    Incrementable,
};
use crate::intrinsics::IntrinsicValueKind;
use crate::ir::const_eval::Value;

use bumpalo::Bump;
use once_cell::unsync::OnceCell;
use std::backtrace::Backtrace;

use std::cell::{Cell, RefCell};
use std::fmt::{Debug, Display, Formatter};
use std::hash::{Hash, Hasher};

pub struct IrCtx<'ir> {
    pub arena: Bump,
    pub counter: Cell<usize>,
    types: RefCell<HashSet<TyP<'ir>>>,
}

impl<'ir> IrCtx<'ir> {
    pub fn new() -> Self {
        Self {
            arena: Bump::new(),
            counter: Cell::new(0),
            types: RefCell::new(HashSet::default()),
        }
    }

    pub fn make_id(&self) -> IrId {
        IrId {
            id: self.counter.increment(),
        }
    }

    pub fn intern_type(&'ir self, ty: Ty<'ir>) -> TyP<'ir> {
        if let Some(key) = self.types.borrow().get(&ty) {
            return key;
        }

        let inner = self.arena.alloc(ty);
        self.types.borrow_mut().insert(inner);

        inner
    }

    pub fn make_symbol(&'ir self) -> IRItemP<'ir> {
        self.arena.alloc(IRItemCell {
            id: self.make_id(),
            contents: OnceCell::new(),
        })
    }
}

impl<'ir, T: Allocatable> ArenaAllocatable<'ir, IrCtx<'ir>> for T
where
    T: 'ir,
{
    type ReturnType = &'ir T;

    fn alloc_on(self, ctx: &'ir IrCtx<'ir>) -> Self::ReturnType {
        ctx.arena.alloc(self)
    }
}

impl<'ir, T: Allocatable + Copy> ArenaAllocatable<'ir, IrCtx<'ir>> for &'_ [T]
where
    T: 'ir,
{
    type ReturnType = &'ir [T];

    fn alloc_on(self, ctx: &'ir IrCtx<'ir>) -> Self::ReturnType {
        ctx.arena.alloc_slice_copy(self)
    }
}

impl<'ir> ArenaAllocatable<'ir, IrCtx<'ir>> for &str {
    type ReturnType = &'ir str;

    fn alloc_on(self, ctx: &'ir IrCtx<'ir>) -> Self::ReturnType {
        ctx.arena.alloc_str(self)
    }
}

impl<'ir, T: Allocatable> ArenaAllocatable<'ir, IrCtx<'ir>> for Vec<T>
where
    T: 'ir,
{
    type ReturnType = &'ir [T];

    fn alloc_on(self, ctx: &'ir IrCtx<'ir>) -> Self::ReturnType {
        ctx.arena.alloc_slice_fill_iter(self)
    }
}

#[derive(PartialEq, Copy, Clone, Eq, Hash, PartialOrd, Ord)]
pub struct IrId {
    pub id: usize,
}

impl Display for IrId {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "${}", self.id)
    }
}

impl Debug for IrId {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        Display::fmt(self, f)
    }
}

#[derive(PartialEq, Eq, Clone, Hash, Copy)]
pub enum Ty<'ir> {
    Item(IRItemP<'ir>),
    Builtin(BuiltinType),
    Pointer(TyP<'ir>, bool),
    Array(TyP<'ir>, usize),
    Tuple(&'ir [TyP<'ir>]),
    FunctionPointer(&'ir [TyP<'ir>], TyP<'ir>),
}

impl Debug for Ty<'_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Ty::Item(cell) => {
                let inner = cell.get();
                match inner {
                    Ok(IRItem::StructLike(s)) => {
                        write!(f, "{} {{ ", s.name.unwrap_or("(unnamed)"))?;
                        for field in s.fields {
                            write!(f, "{:?} ", field.ty)?;
                        }
                        write!(f, "}}")
                    }
                    Ok(IRItem::Enum(e)) => {
                        write!(f, "{}", e.name.unwrap_or("(unnamed enum)"))
                    }
                    Ok(IRItem::Protocol(s)) => {
                        write!(f, "{}", s.name.unwrap_or("(unnamed protocol)"))
                    }
                    Ok(IRItem::Function(s)) => {
                        write!(f, "{}", s.name.unwrap_or("(unnamed function)"))
                    }
                    Ok(IRItem::Closure(_)) => {
                        write!(f, "(closure)")
                    }
                    _ => write!(f, "ERROR"),
                }
            }
            Ty::Builtin(builtin) => write!(f, "{:?}", builtin),
            Ty::Pointer(ty, is_const) => {
                write!(f, "&{}{:?}", if *is_const { "" } else { "mut " }, ty)
            }
            Ty::Array(ty, len) => write!(f, "[{:?}; {}]", ty, len),
            Ty::Tuple(tys) => {
                write!(f, "(")?;
                for (i, ty) in tys.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{:?}", ty)?;
                }
                write!(f, ")")
            }
            Ty::FunctionPointer(args, ret) => {
                write!(f, "fn(")?;
                for (i, arg) in args.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{:?}", arg)?;
                }
                write!(f, ") -> {:?}", ret)
            }
        }
    }
}

impl<'ir> Ty<'ir> {
    pub fn void() -> Ty<'ir> {
        Ty::Tuple(&[])
    }

    /// Returns true if lhs <= rhs on the stric type hierarchy (implicit coercions are not
    /// considered).
    pub fn assignable_from(&self, other: &Ty<'ir>) -> bool {
        match (self, other) {
            _ if self == other => true,
            (Ty::Pointer(a, true), Ty::Pointer(b, _)) if a == b => true,
            (_, Ty::Builtin(BuiltinType::Never)) => true,
            _ => false,
        }
    }

    pub fn gcd(lhs: &Ty<'ir>, rhs: &Ty<'ir>) -> Ty<'ir> {
        match (lhs, rhs) {
            _ if lhs == rhs => *lhs,
            (Ty::Pointer(a, false), Ty::Pointer(b, _)) if a == b => Ty::Pointer(a, false),
            (Ty::Pointer(a, _), Ty::Pointer(b, false)) if a == b => Ty::Pointer(a, false),
            (_, Ty::Builtin(BuiltinType::Never)) => *lhs,
            (Ty::Builtin(BuiltinType::Never), _) => *rhs,
            _ => Ty::void(),
        }
    }

    pub fn canonical_type(&'ir self) -> TyP<'ir> {
        match self {
            Ty::Pointer(inner, _) => inner.canonical_type(),
            _ => self,
        }
    }

    pub fn is_void(&self) -> bool {
        matches!(self, Ty::Tuple(tys) if tys.is_empty())
    }

    pub fn is_never(&self) -> bool {
        matches!(self, Ty::Builtin(BuiltinType::Never))
    }

    pub fn is_zero_sized(&self) -> bool {
        match self {
            Ty::Builtin(BuiltinType::Never) => true,
            Ty::Builtin(_) => false,
            Ty::Item(inner) => match inner.get().unwrap() {
                IRItem::Alias(inner) => inner.is_zero_sized(),
                IRItem::StructLike(s) => s.fields.iter().all(|f| f.ty.is_zero_sized()),
                IRItem::Closure(c) => c.data.fields.iter().all(|f| f.ty.is_zero_sized()),
                IRItem::Function(_) => true,
                IRItem::Enum(e) => e.underlying_type.is_zero_sized(),
                IRItem::Protocol(_) => unreachable!(),
                IRItem::Static(_) => unreachable!(),
                IRItem::Const(_) => unreachable!(),
            },
            Ty::Pointer(_, _) => false,
            Ty::Array(inner, size) => *size == 0 || inner.is_zero_sized(),
            Ty::Tuple(elems) => elems.iter().all(|e| e.is_zero_sized()),
            Ty::FunctionPointer(_, _) => false,
        }
    }
}

pub type TyP<'ir> = &'ir Ty<'ir>;

#[derive(Debug, PartialEq, Eq, Clone, Hash, Copy)]
pub struct Field<'ir> {
    pub id: IrId,
    pub ty: TyP<'ir>,
}

#[derive(Debug)]
pub struct StructLike<'ir> {
    pub name: Option<&'ir str>,
    pub attributes: &'ir [Attribute],
    pub fields: &'ir [Field<'ir>],
    pub is_union: bool,
}

#[derive(Debug, Clone, Copy)]
pub struct Parameter<'ir> {
    pub id: IrId,
    pub ty: TyP<'ir>,
}

#[derive(Debug, Copy, Clone)]
pub struct LocalDef<'ir> {
    pub id: IrId,
    pub typ: TyP<'ir>,
}

#[derive(Debug)]
pub struct FuncBody<'ir> {
    pub local_defs: &'ir [LocalDef<'ir>],
    pub statements: &'ir [Statement<'ir>],
    pub raw_body: Option<ExprP<'ir>>,
}

#[derive(Debug)]
pub struct Function<'ir> {
    pub name: Option<&'ir str>,
    pub attributes: &'ir [Attribute],
    pub args: &'ir [Parameter<'ir>],
    pub return_type: TyP<'ir>,
    pub body: OnceCell<FuncBody<'ir>>,
    pub varargs: bool,
}

#[derive(Debug)]
pub struct Closure<'ir> {
    pub data: StructLike<'ir>,
    pub function: OnceCell<IRItemP<'ir>>,
}

#[derive(Debug)]
pub struct Protocol<'ir> {
    pub name: Option<&'ir str>,
    pub methods: &'ir [ProtocolFunction<'ir>],
}

#[derive(Debug)]
pub struct ProtocolFunction<'ir> {
    pub name: &'ir str,
    pub arg_types: &'ir [TyP<'ir>],
    pub return_type: TyP<'ir>,
}

#[derive(Debug)]
pub struct VtableLayout<'ir> {
    pub methods: &'ir [ProtocolFunction<'ir>],
}

#[derive(Debug)]
pub struct EnumMember<'ir> {
    pub id: IrId,
    pub name: &'ir str,
    pub value: ExprP<'ir>,
}

#[derive(Debug)]
pub struct Enum<'ir> {
    pub name: Option<&'ir str>,
    pub underlying_type: TyP<'ir>,
    pub members: &'ir [EnumMember<'ir>],
}

#[derive(Debug)]
pub struct Static<'ir> {
    pub name: Option<&'ir str>,
    pub typ: TyP<'ir>,
    pub init: Option<ExprP<'ir>>,
    pub attributes: &'ir [Attribute],
    pub r#extern: bool,
}

#[derive(Debug)]
pub struct Const<'ir> {
    pub name: Option<&'ir str>,
    pub typ: TyP<'ir>,
    pub value: Value<'ir>,
    pub init: ExprP<'ir>,
}

#[derive(Debug)]
pub struct StructInit<'ir> {
    pub field: IrId,
    pub value: ExprP<'ir>,
}

#[derive(Debug)]
pub struct TupleInit<'ir> {
    pub index: usize,
    pub value: ExprP<'ir>,
}

#[derive(Debug)]
pub enum IRItem<'ir> {
    StructLike(StructLike<'ir>),
    Alias(TyP<'ir>),
    Protocol(Protocol<'ir>),
    Function(Function<'ir>),
    Enum(Enum<'ir>),
    Static(Static<'ir>),
    Const(Const<'ir>),
    Closure(Closure<'ir>),
}

pub type IRItemP<'ir> = &'ir IRItemCell<'ir>;

impl<'ir> IRItemCell<'ir> {
    pub fn assign(&self, value: IRItem<'ir>) {
        // Panic if we try to assign the same symbol twice
        self.contents
            .set(value)
            .expect("assigning the same symbol twice");
    }

    pub fn get(&'ir self) -> Result<&'ir IRItem<'ir>, CodeErrorKind> {
        match self.contents.get() {
            Some(item) => Ok(item),
            None => Err(CodeErrorKind::UnpopulatedSymbol),
        }
    }

    pub fn get_alias(&'ir self) -> Option<TyP<'ir>> {
        match self.contents.get() {
            Some(IRItem::Alias(ty)) => Some(*ty),
            _ => None,
        }
    }

    pub fn get_function(&'ir self) -> Result<&'ir Function<'ir>, CodeErrorKind> {
        match self.contents.get() {
            Some(IRItem::Function(f)) => Ok(f),
            Some(_) => Err(CodeErrorKind::InternalError(
                "function expected".into(),
                Backtrace::capture().into(),
            )),
            None => Err(CodeErrorKind::UnpopulatedSymbol),
        }
    }

    pub fn get_closure(&'ir self) -> Result<&'ir Closure<'ir>, CodeErrorKind> {
        match self.contents.get() {
            Some(IRItem::Closure(c)) => Ok(c),
            Some(_) => Err(CodeErrorKind::InternalError(
                "closure expected".into(),
                Backtrace::capture().into(),
            )),
            None => Err(CodeErrorKind::UnpopulatedSymbol),
        }
    }

    pub fn get_protocol(&'ir self) -> Result<&'ir Protocol<'ir>, CodeErrorKind> {
        match self.contents.get() {
            Some(IRItem::Protocol(p)) => Ok(p),
            Some(_) => Err(CodeErrorKind::InternalError(
                "protocol expected".into(),
                Backtrace::capture().into(),
            )),
            None => Err(CodeErrorKind::UnpopulatedSymbol),
        }
    }

    pub fn get_struct_like(&'ir self) -> Result<&'ir StructLike<'ir>, CodeErrorKind> {
        match self.contents.get() {
            Some(IRItem::StructLike(p)) => Ok(p),
            Some(_) => Err(CodeErrorKind::InternalError(
                "struct expected".into(),
                Backtrace::capture().into(),
            )),
            None => Err(CodeErrorKind::UnpopulatedSymbol),
        }
    }

    pub fn get_enum(&'ir self) -> Result<&'ir Enum<'ir>, CodeErrorKind> {
        match self.contents.get() {
            Some(IRItem::Enum(p)) => Ok(p),
            Some(_) => Err(CodeErrorKind::InternalError(
                "enum expected".into(),
                Backtrace::capture().into(),
            )),
            None => Err(CodeErrorKind::UnpopulatedSymbol),
        }
    }

    pub fn get_static(&'ir self) -> Result<&'ir Static<'ir>, CodeErrorKind> {
        match self.contents.get() {
            Some(IRItem::Static(s)) => Ok(s),
            Some(_) => Err(CodeErrorKind::InternalError(
                "static expected".into(),
                Backtrace::capture().into(),
            )),
            None => Err(CodeErrorKind::UnpopulatedSymbol),
        }
    }

    pub fn get_const(&'ir self) -> Result<&'ir Const<'ir>, CodeErrorKind> {
        match self.contents.get() {
            Some(IRItem::Const(c)) => Ok(c),
            Some(_) => Err(CodeErrorKind::InternalError(
                "const expected".into(),
                Backtrace::capture().into(),
            )),
            None => Err(CodeErrorKind::UnpopulatedSymbol),
        }
    }

    pub fn is_struct_like(&self) -> bool {
        matches!(self.contents.get(), Some(IRItem::StructLike(_)))
    }
}
pub struct IRItemCell<'ir> {
    pub id: IrId,
    contents: OnceCell<IRItem<'ir>>,
}

impl Hash for IRItemCell<'_> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.id.hash(state);
    }
}

/// Symbols have reference semantics. Two structs with the same fields
/// are not considered equal.
impl PartialEq for IRItemCell<'_> {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}

impl Eq for IRItemCell<'_> {}

impl Debug for IRItemCell<'_> {
    fn fmt(&self, fmt: &mut Formatter<'_>) -> Result<(), std::fmt::Error> {
        if fmt.alternate() {
            writeln!(fmt, "{} {{", self.id)?;
            writeln!(fmt, "\t{:?}", self.contents.get())?;
            writeln!(fmt, "}}")?;
        } else {
            write!(fmt, "{}", self.id)?
        }

        Ok(())
    }
}

#[derive(Debug, Clone)]
pub enum Statement<'ir> {
    Expression(ExprP<'ir>),
    Label(IrId),
}

impl<'ir> Statement<'ir> {
    pub fn pure(&self) -> bool {
        match self {
            Statement::Expression(expr) => expr.pure(),
            Statement::Label(_) => false,
        }
    }
}

#[derive(Debug, Clone)]
pub enum ExprKind<'ir> {
    Block(&'ir [Statement<'ir>], ExprP<'ir>),
    Binary(BinOp, ExprP<'ir>, ExprP<'ir>),
    AssignOp(BinOp, ExprP<'ir>, ExprP<'ir>),
    Call(ExprP<'ir>, &'ir [ExprP<'ir>]),
    Fn(IRItemP<'ir>),
    Ref(ExprP<'ir>),
    Deref(ExprP<'ir>),
    Return(ExprP<'ir>),
    Goto(IrId),
    Unary(UnOp, ExprP<'ir>),
    Assign(ExprP<'ir>, ExprP<'ir>),
    Index(ExprP<'ir>, ExprP<'ir>),
    Local(IrId),
    Static(IRItemP<'ir>),
    Const(IRItemP<'ir>),
    Literal(const_eval::Value<'ir>),
    Field(ExprP<'ir>, IrId),
    TupleIndex(ExprP<'ir>, usize),
    If(ExprP<'ir>, ExprP<'ir>, ExprP<'ir>, Option<bool>),
    Cast(ExprP<'ir>),

    Intrinsic(IntrinsicValueKind<'ir>),

    Array(&'ir [ExprP<'ir>]),
    Tuple(&'ir [TupleInit<'ir>]),
    Struct(&'ir [StructInit<'ir>]),

    Unreachable,
    Void,
}

#[derive(Debug, PartialEq, Eq, Clone, Hash, Copy)]
pub enum ValueType {
    LValue,
    RValue,
}

#[derive(Debug, Clone)]
pub struct Expr<'ir> {
    pub value_type: ValueType,
    pub is_const: bool,
    pub kind: ExprKind<'ir>,
    pub span: Option<Span>,
    pub ty: TyP<'ir>,
}

impl<'ir> Expr<'ir> {
    pub fn lvalue(kind: ExprKind<'ir>, typ: TyP<'ir>, span: Option<Span>) -> Self {
        Self {
            kind,
            value_type: ValueType::LValue,
            is_const: false,
            ty: typ,
            span,
        }
    }

    pub fn rvalue(kind: ExprKind<'ir>, typ: TyP<'ir>, span: Option<Span>) -> Self {
        Self {
            kind,
            value_type: ValueType::RValue,
            is_const: false,
            ty: typ,
            span,
        }
    }

    pub fn const_lvalue(kind: ExprKind<'ir>, typ: TyP<'ir>, span: Option<Span>) -> Self {
        Self {
            kind,
            value_type: ValueType::LValue,
            is_const: true,
            ty: typ,
            span,
        }
    }

    pub fn diverges(&self) -> bool {
        match self.value_type {
            ValueType::LValue => matches!(self.ty, Ty::Builtin(BuiltinType::Never)), //false,
            ValueType::RValue => matches!(self.ty, Ty::Builtin(BuiltinType::Never)),
        }
    }

    pub fn is_void(&self) -> bool {
        matches!(self.kind, ExprKind::Void)
    }

    pub fn is_unreachable(&self) -> bool {
        matches!(self.kind, ExprKind::Unreachable)
    }

    pub fn pure(&self) -> bool {
        match self.kind {
            ExprKind::Block(stmts, e) => stmts.iter().all(|s| s.pure()) && e.pure(),
            ExprKind::Binary(_, a, b) => a.pure() && b.pure(),
            ExprKind::Ref(inner) => inner.pure(),
            ExprKind::Deref(inner) => inner.pure(),
            ExprKind::Unary(_, inner) => inner.pure(),
            ExprKind::Index(a, b) => a.pure() && b.pure(),
            ExprKind::If(a, b, c, _) => a.pure() && b.pure() && c.pure(),
            ExprKind::Cast(inner) => inner.pure(),
            ExprKind::Field(inner, _) => inner.pure(),
            ExprKind::TupleIndex(inner, _) => inner.pure(),
            ExprKind::Array(inner) => inner.iter().all(|e| e.pure()),
            ExprKind::Tuple(inner) => inner.iter().all(|e| e.value.pure()),
            ExprKind::Struct(inner) => inner.iter().all(|e| e.value.pure()),

            ExprKind::Fn(_) => true,
            ExprKind::Local(_) => true,
            ExprKind::Static(_) => true,
            ExprKind::Const(_) => true,
            ExprKind::Literal(_) => true,
            ExprKind::Void => true,

            ExprKind::Intrinsic(ref kind) => match kind {
                IntrinsicValueKind::SizeOfLike(_, _) => true,
                IntrinsicValueKind::Dangling(_) => true,
                IntrinsicValueKind::Asm(_) => false,
                IntrinsicValueKind::FunctionLike(_) => false,
                IntrinsicValueKind::ConstLike(_) => false,
                IntrinsicValueKind::Uninitialized => true,
                IntrinsicValueKind::InConstContext => true,
                IntrinsicValueKind::ConstPanic(_) => false,
                IntrinsicValueKind::ConstWrite(_, _) => false,
                IntrinsicValueKind::ConstAlloc(_, _) => false,
                IntrinsicValueKind::ConstFree(_) => false,
            },

            ExprKind::Unreachable => false, // ?
            ExprKind::Call(_, _) => false,  // for now
            ExprKind::Assign(_, _) => false,
            ExprKind::AssignOp(_, _, _) => false,
            ExprKind::Return(_) => false,
            ExprKind::Goto(_) => false,
        }
    }
}

pub trait ExpressionVisitor<'ir>: Sized {
    fn visit_statement(&mut self, stmt: &Statement<'ir>) -> Result<(), AluminaError> {
        match stmt {
            Statement::Expression(expr) => self.visit_expr(expr),
            Statement::Label(id) => self.visit_label(*id),
        }
    }

    fn visit_label(&mut self, _label: IrId) -> Result<(), AluminaError> {
        Ok(())
    }

    fn visit_block(
        &mut self,
        block: &'ir [Statement<'ir>],
        expr: ExprP<'ir>,
    ) -> Result<(), AluminaError> {
        for stmt in block {
            self.visit_statement(stmt)?;
        }
        self.visit_expr(expr)
    }

    fn visit_binary(
        &mut self,
        _op: BinOp,
        a: ExprP<'ir>,
        b: ExprP<'ir>,
    ) -> Result<(), AluminaError> {
        self.visit_expr(a)?;
        self.visit_expr(b)
    }

    fn visit_assign_op(
        &mut self,
        _op: BinOp,
        lhs: ExprP<'ir>,
        rhs: ExprP<'ir>,
    ) -> Result<(), AluminaError> {
        self.visit_expr(lhs)?;
        self.visit_expr(rhs)
    }

    fn visit_call(
        &mut self,
        callee: ExprP<'ir>,
        args: &'ir [ExprP<'ir>],
    ) -> Result<(), AluminaError> {
        self.visit_expr(callee)?;
        for arg in args {
            self.visit_expr(arg)?;
        }
        Ok(())
    }

    fn visit_fn(&mut self, _item: IRItemP<'ir>) -> Result<(), AluminaError> {
        Ok(())
    }

    fn visit_ref(&mut self, inner: ExprP<'ir>) -> Result<(), AluminaError> {
        self.visit_expr(inner)
    }

    fn visit_deref(&mut self, inner: ExprP<'ir>) -> Result<(), AluminaError> {
        self.visit_expr(inner)
    }

    fn visit_return(&mut self, expr: ExprP<'ir>) -> Result<(), AluminaError> {
        self.visit_expr(expr)
    }

    fn visit_goto(&mut self, _label: IrId) -> Result<(), AluminaError> {
        Ok(())
    }

    fn visit_unary(&mut self, _op: UnOp, inner: ExprP<'ir>) -> Result<(), AluminaError> {
        self.visit_expr(inner)
    }

    fn visit_assign(&mut self, lhs: ExprP<'ir>, rhs: ExprP<'ir>) -> Result<(), AluminaError> {
        self.visit_expr(lhs)?;
        self.visit_expr(rhs)
    }

    fn visit_index(&mut self, lhs: ExprP<'ir>, rhs: ExprP<'ir>) -> Result<(), AluminaError> {
        self.visit_expr(lhs)?;
        self.visit_expr(rhs)
    }

    fn visit_local(&mut self, _id: IrId) -> Result<(), AluminaError> {
        Ok(())
    }

    fn visit_static(&mut self, _item: IRItemP<'ir>) -> Result<(), AluminaError> {
        Ok(())
    }

    fn visit_const(&mut self, _item: IRItemP<'ir>) -> Result<(), AluminaError> {
        Ok(())
    }

    fn visit_literal(&mut self, _value: &const_eval::Value<'ir>) -> Result<(), AluminaError> {
        Ok(())
    }

    fn visit_field(&mut self, expr: ExprP<'ir>, _id: IrId) -> Result<(), AluminaError> {
        self.visit_expr(expr)
    }

    fn visit_tuple_index(&mut self, expr: ExprP<'ir>, _index: usize) -> Result<(), AluminaError> {
        self.visit_expr(expr)
    }

    fn visit_if(
        &mut self,
        cond: ExprP<'ir>,
        then: ExprP<'ir>,
        els: ExprP<'ir>,
        _const_cond: Option<bool>,
    ) -> Result<(), AluminaError> {
        self.visit_expr(cond)?;
        self.visit_expr(then)?;
        self.visit_expr(els)
    }

    fn visit_cast(&mut self, expr: ExprP<'ir>) -> Result<(), AluminaError> {
        self.visit_expr(expr)
    }

    fn visit_codegen_intrinsic(
        &mut self,
        _kind: &IntrinsicValueKind<'ir>,
    ) -> Result<(), AluminaError> {
        Ok(())
    }

    fn visit_array(&mut self, exprs: &'ir [ExprP<'ir>]) -> Result<(), AluminaError> {
        for expr in exprs {
            self.visit_expr(expr)?;
        }
        Ok(())
    }

    fn visit_tuple(&mut self, exprs: &'ir [TupleInit<'ir>]) -> Result<(), AluminaError> {
        for expr in exprs {
            self.visit_expr(expr.value)?;
        }
        Ok(())
    }

    fn visit_struct(&mut self, exprs: &'ir [StructInit<'ir>]) -> Result<(), AluminaError> {
        for expr in exprs {
            self.visit_expr(expr.value)?;
        }
        Ok(())
    }

    fn visit_unreachable(&mut self) -> Result<(), AluminaError> {
        Ok(())
    }

    fn visit_void(&mut self) -> Result<(), AluminaError> {
        Ok(())
    }

    fn visit_expr(&mut self, expr: ExprP<'ir>) -> Result<(), AluminaError> {
        default_visit_expr(self, expr)
    }
}

pub fn default_visit_expr<'ir, V: ExpressionVisitor<'ir>>(
    visitor: &mut V,
    expr: ExprP<'ir>,
) -> Result<(), AluminaError> {
    match &expr.kind {
        ExprKind::Block(block, expr) => visitor.visit_block(block, expr),
        ExprKind::Binary(op, a, b) => visitor.visit_binary(*op, a, b),
        ExprKind::AssignOp(op, lhs, rhs) => visitor.visit_assign_op(*op, lhs, rhs),
        ExprKind::Call(callee, args) => visitor.visit_call(callee, args),
        ExprKind::Fn(item) => visitor.visit_fn(item),
        ExprKind::Ref(inner) => visitor.visit_ref(inner),
        ExprKind::Deref(inner) => visitor.visit_deref(inner),
        ExprKind::Return(expr) => visitor.visit_return(expr),
        ExprKind::Goto(label) => visitor.visit_goto(*label),
        ExprKind::Unary(op, inner) => visitor.visit_unary(*op, inner),
        ExprKind::Assign(lhs, rhs) => visitor.visit_assign(lhs, rhs),
        ExprKind::Index(lhs, rhs) => visitor.visit_index(lhs, rhs),
        ExprKind::Local(id) => visitor.visit_local(*id),
        ExprKind::Static(item) => visitor.visit_static(item),
        ExprKind::Const(item) => visitor.visit_const(item),
        ExprKind::Literal(value) => visitor.visit_literal(value),
        ExprKind::Field(expr, id) => visitor.visit_field(expr, *id),
        ExprKind::TupleIndex(expr, index) => visitor.visit_tuple_index(expr, *index),
        ExprKind::If(cond, then, els, const_cond) => visitor.visit_if(cond, then, els, *const_cond),
        ExprKind::Cast(expr) => visitor.visit_cast(expr),
        ExprKind::Intrinsic(kind) => visitor.visit_codegen_intrinsic(kind),
        ExprKind::Array(exprs) => visitor.visit_array(exprs),
        ExprKind::Tuple(exprs) => visitor.visit_tuple(exprs),
        ExprKind::Struct(exprs) => visitor.visit_struct(exprs),
        ExprKind::Unreachable => visitor.visit_unreachable(),
        ExprKind::Void => visitor.visit_void(),
    }
}

pub type ExprP<'ir> = &'ir Expr<'ir>;

impl_allocatable!(
    Expr<'_>,
    Ty<'_>,
    Statement<'_>,
    Field<'_>,
    Parameter<'_>,
    IRItemCell<'_>,
    EnumMember<'_>,
    ProtocolFunction<'_>,
    LocalDef<'_>,
    StructInit<'_>,
    TupleInit<'_>,
    const_eval::Value<'_>,
    const_eval::LValue<'_>,
    IrId
);
