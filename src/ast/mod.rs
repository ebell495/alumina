pub mod expressions;
pub mod maker;
pub mod types;

use crate::common::{Allocatable, ArenaAllocatable, Incrementable};
use std::fmt::Display;
use std::fmt::{Debug, Formatter};
use std::hash::{Hash, Hasher};

use bumpalo::Bump;
use once_cell::unsync::OnceCell;
use std::cell::{Cell, RefCell};
use std::collections::HashSet;

use crate::common::impl_allocatable;

pub struct AstCtx<'ast> {
    pub arena: Bump,
    pub counter: Cell<usize>,
    types: RefCell<HashSet<TyP<'ast>>>,
}

impl<'ast> AstCtx<'ast> {
    pub fn new() -> Self {
        Self {
            arena: Bump::new(),
            counter: Cell::new(0),
            types: RefCell::new(HashSet::new()),
        }
    }

    pub fn make_id(&self) -> AstId {
        AstId {
            id: self.counter.increment(),
        }
    }

    pub fn intern_type(&'ast self, ty: Ty<'ast>) -> TyP<'ast> {
        if let Some(key) = self.types.borrow().get(&ty) {
            return *key;
        }

        let inner = self.arena.alloc(ty);
        self.types.borrow_mut().insert(inner);

        inner
    }

    pub fn make_symbol(&'ast self) -> ItemP<'ast> {
        let inner = self.arena.alloc(ItemCell {
            id: self.make_id(),
            contents: OnceCell::new(),
        });

        inner
    }
}

impl<'gcx, T: Allocatable> ArenaAllocatable<'gcx, AstCtx<'gcx>> for T
where
    T: 'gcx,
{
    type ReturnType = &'gcx T;

    fn alloc_on(self, ctx: &'gcx AstCtx<'gcx>) -> Self::ReturnType {
        ctx.arena.alloc(self)
    }
}

impl<'gcx, T: Allocatable + Copy> ArenaAllocatable<'gcx, AstCtx<'gcx>> for &'_ [T]
where
    T: 'gcx,
{
    type ReturnType = &'gcx [T];

    fn alloc_on(self, ctx: &'gcx AstCtx<'gcx>) -> Self::ReturnType {
        ctx.arena.alloc_slice_copy(self)
    }
}

impl<'gcx> ArenaAllocatable<'gcx, AstCtx<'gcx>> for &str {
    type ReturnType = &'gcx str;

    fn alloc_on(self, ctx: &'gcx AstCtx<'gcx>) -> Self::ReturnType {
        ctx.arena.alloc_str(self)
    }
}

impl<'gcx, T: Allocatable> ArenaAllocatable<'gcx, AstCtx<'gcx>> for Vec<T>
where
    T: 'gcx,
{
    type ReturnType = &'gcx [T];

    fn alloc_on(self, ctx: &'gcx AstCtx<'gcx>) -> Self::ReturnType {
        ctx.arena.alloc_slice_fill_iter(self)
    }
}

#[derive(PartialEq, Copy, Clone, Eq, Hash)]
pub struct AstId {
    pub id: usize,
}

impl Display for AstId {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "${}", self.id)
    }
}

impl Debug for AstId {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        Display::fmt(self, f)
    }
}

#[derive(Debug, PartialEq, Copy, Clone, Eq, Hash)]
pub enum BuiltinType {
    Void,
    Never,
    Bool,
    U8,
    U16,
    U32,
    U64,
    U128,
    USize,
    ISize,
    I8,
    I16,
    I32,
    I64,
    I128,
    F32,
    F64,
}

#[derive(Debug, PartialEq, Eq, Clone, Hash)]
pub enum Ty<'ast> {
    Placeholder(AstId),
    Extern(AstId),
    NamedType(ItemP<'ast>),
    Builtin(BuiltinType),
    Pointer(TyP<'ast>),
    Array(TyP<'ast>, usize),
    Slice(TyP<'ast>),
    Tuple(&'ast [TyP<'ast>]),
    Function(&'ast [TyP<'ast>], TyP<'ast>),
    GenericType(ItemP<'ast>, &'ast [TyP<'ast>]),
}

pub type TyP<'ast> = &'ast Ty<'ast>;

#[derive(Debug)]
pub enum Item<'ast> {
    Struct(Struct<'ast>),
    Function(Function<'ast>),
}

pub type ItemP<'ast> = &'ast ItemCell<'ast>;

impl<'ast> ItemCell<'ast> {
    pub fn assign(&self, value: Item<'ast>) {
        // Panic if we try to assign the same symbol twice
        self.contents.set(value).unwrap();
    }

    pub fn get(&'ast self) -> &'ast Item<'ast> {
        self.contents.get().unwrap()
    }
}

/// SymbolCell is a wrapper that allows us to build recursive structures incrementally.
/// This allows us to assign symbols to syntax early in name resolution and fill them in
/// later.
/// Symbols are immutable once they are assigned.
pub struct ItemCell<'ast> {
    pub id: AstId,
    pub contents: OnceCell<Item<'ast>>,
}

impl Hash for ItemCell<'_> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.id.hash(state);
    }
}

/// Symbols have reference semantics. Two structs with the same fields
/// are not considered equal.
impl PartialEq for ItemCell<'_> {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}

impl Eq for ItemCell<'_> {}

impl Debug for ItemCell<'_> {
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

#[derive(Debug, Clone, Copy)]
pub struct Field<'ast> {
    pub name: &'ast str,
    pub ty: TyP<'ast>,
}

#[derive(Debug)]
pub struct Struct<'ast> {
    pub placeholders: &'ast [AstId],
    pub associated_fns: &'ast [ItemP<'ast>],
    pub fields: &'ast [Field<'ast>],
}

#[derive(Debug, Clone, Copy)]
pub struct Parameter<'ast> {
    pub id: AstId,
    pub ty: TyP<'ast>,
}

#[derive(Debug)]
pub struct Function<'ast> {
    pub placeholders: &'ast [AstId],
    pub parameters: &'ast [Parameter<'ast>],
    pub return_type: TyP<'ast>,
    pub body: Option<ExprP<'ast>>,
}

#[derive(Debug, PartialEq, Eq, Clone, Hash)]
pub struct LetDeclaration<'ast> {
    pub id: AstId,
    pub typ: Option<TyP<'ast>>,
    pub value: Option<ExprP<'ast>>,
}

#[derive(Debug, PartialEq, Eq, Clone, Hash)]
pub enum Statement<'ast> {
    Expression(ExprP<'ast>),
    LetDeclaration(LetDeclaration<'ast>),
}

#[derive(Debug, PartialEq, Eq, Clone, Copy, Hash)]
pub enum BinOp {
    And,
    Or,
    BitAnd,
    BitOr,
    BitXor,
    Eq,
    Neq,
    Lt,
    LEq,
    Gt,
    GEq,
    LShift,
    Rsh,
    Plus,
    Minus,
    Mul,
    Div,
    Mod,
}

#[derive(Debug, PartialEq, Eq, Clone, Copy, Hash)]
pub enum UnOp {
    Neg,
    Not,
}

#[derive(Debug, PartialEq, Eq, Clone, Hash)]
pub enum Lit<'ast> {
    Str(&'ast str),
    Byte(u8),
    Int(u128),
    Float(&'ast str),
    Bool(bool),
    Null,
}

#[derive(Debug, PartialEq, Eq, Clone, Hash)]
pub enum Expr<'ast> {
    Block(&'ast [Statement<'ast>], ExprP<'ast>),
    Binary(ExprP<'ast>, BinOp, ExprP<'ast>),
    Call(ExprP<'ast>, &'ast [ExprP<'ast>]),
    Function(ItemP<'ast>),
    Ref(ExprP<'ast>),
    Deref(ExprP<'ast>),
    Unary(UnOp, ExprP<'ast>),
    Assign(ExprP<'ast>, ExprP<'ast>),
    AssignOp(BinOp, ExprP<'ast>, ExprP<'ast>),
    Local(AstId),
    Lit(Lit<'ast>),
    Tuple(&'ast [ExprP<'ast>]),
    Field(ExprP<'ast>, &'ast str),
    TupleIndex(ExprP<'ast>, usize),
    If(ExprP<'ast>, ExprP<'ast>, ExprP<'ast>),
    Cast(ExprP<'ast>, TyP<'ast>),

    // Generics support
    DeferredFunction(AstId, &'ast str),
    GenericFunction(ExprP<'ast>, &'ast [TyP<'ast>]),

    Void,
}

pub type ExprP<'ast> = &'ast Expr<'ast>;

impl_allocatable!(
    Expr<'_>,
    Ty<'_>,
    Statement<'_>,
    Field<'_>,
    Parameter<'_>,
    ItemCell<'_>,
    AstId
);