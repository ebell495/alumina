use crate::ast::{AstCtx, Attribute, ItemP, MacroCtx, Span};
use crate::common::{
    AluminaError, ArenaAllocatable, CodeError, CodeErrorKind, IndexMap, Marker,
    WithSpanDuringParsing,
};
use crate::global_ctx::GlobalCtx;
use crate::name_resolution::path::Path;
use crate::name_resolution::scope::{NamedItem, NamedItemKind, Scope, ScopeType};
use crate::parser::{AluminaVisitor, FieldKind, NodeExt, ParseCtx};
use crate::visitors::{AttributeVisitor, UseClauseVisitor, VisitorExt};

use std::result::Result;
use tree_sitter::Node;

type ItemMap<'ast, 'src> =
    IndexMap<(Scope<'ast, 'src>, Option<&'ast str>), Vec<NamedItem<'ast, 'src>>>;

pub struct FirstPassVisitor<'ast, 'src> {
    global_ctx: GlobalCtx,
    ast: &'ast AstCtx<'ast>,
    scope: Scope<'ast, 'src>,
    code: &'src ParseCtx<'src>,
    enum_item: Option<ItemP<'ast>>,

    in_a_container: bool,
    main_module_path: Option<Path<'ast>>,
    main_candidate: Option<ItemP<'ast>>,

    items: ItemMap<'ast, 'src>,
    macro_ctx: MacroCtx,
}

impl<'ast, 'src> FirstPassVisitor<'ast, 'src> {
    pub fn new(
        global_ctx: GlobalCtx,
        ast: &'ast AstCtx<'ast>,
        scope: Scope<'ast, 'src>,
        macro_ctx: MacroCtx,
    ) -> Self {
        Self {
            global_ctx,
            ast,
            code: scope
                .code()
                .expect("cannot run on scope without parse context"),
            scope,
            in_a_container: false,
            enum_item: None,
            main_module_path: None,
            main_candidate: None,
            items: IndexMap::default(),
            macro_ctx,
        }
    }

    pub fn with_main(
        global_ctx: GlobalCtx,
        ast: &'ast AstCtx<'ast>,
        scope: Scope<'ast, 'src>,
        macro_ctx: MacroCtx,
    ) -> Self {
        Self {
            global_ctx,
            ast,
            code: scope
                .code()
                .expect("cannot run on scope without parse context"),
            main_module_path: Some(scope.path()),
            scope,
            in_a_container: false,
            enum_item: None,
            main_candidate: None,
            items: IndexMap::default(),
            macro_ctx,
        }
    }

    pub fn main_candidate(&self) -> Option<ItemP<'ast>> {
        self.main_candidate
    }

    pub fn visit_local(mut self, node: Node<'src>) -> Result<ItemMap<'ast, 'src>, AluminaError> {
        self.visit(node)?;
        Ok(self.items)
    }

    fn add_item(
        &mut self,
        node: Node<'src>,
        name: &'ast str,
        item: NamedItem<'ast, 'src>,
    ) -> Result<(), AluminaError> {
        if let Some(id) = item.ast_id() {
            self.ast.add_local_name(id, name)
        }
        self.scope
            .add_item(Some(name), item.clone())
            .with_span_from(&self.scope, node)?;
        self.items
            .entry((self.scope.clone(), Some(name)))
            .or_default()
            .push(item);
        Ok(())
    }

    fn add_unnamed_item(
        &mut self,
        node: Node<'src>,
        item: NamedItem<'ast, 'src>,
    ) -> Result<(), AluminaError> {
        self.scope
            .add_item(None, item.clone())
            .with_span_from(&self.scope, node)?;
        self.items
            .entry((self.scope.clone(), None))
            .or_default()
            .push(item);
        Ok(())
    }
}

macro_rules! with_child_scope {
    ($self:ident, $scope:expr, $body:block) => {
        let previous_scope = std::mem::replace(&mut $self.scope, $scope);
        $body
        $self.scope = previous_scope;
    };
}

macro_rules! with_child_scope_container {
    ($self:ident, $scope:expr, $body:block) => {
        let previous_scope = std::mem::replace(&mut $self.scope, $scope);
        let previous_in_a_container = $self.in_a_container;
        $body
        $self.scope = previous_scope;
        $self.in_a_container = previous_in_a_container;
    };
}

impl<'ast, 'src> FirstPassVisitor<'ast, 'src> {
    fn parse_name(&self, node: Node<'src>) -> &'ast str {
        let name_node = node.child_by_field(FieldKind::Name).unwrap();
        self.code.node_text(name_node).alloc_on(self.ast)
    }
}

macro_rules! parse_attributes {
    (@, $self:expr, $node:expr, $item:expr) => {
        match AttributeVisitor::parse_attributes($self.global_ctx.clone(), $self.ast, $self.scope.clone(), $node, $item)? {
            Some(attributes) => attributes,
            None => return Ok(()),
        }
    };
    ($self:expr, $node:expr, $item:expr) => {
        parse_attributes!(@, $self, $node, Some($item))
    };
    ($self:expr, $node:expr) => {
        parse_attributes!(@, $self, $node, None)
    };
}

pub(crate) use parse_attributes;

impl<'ast, 'src> AluminaVisitor<'src> for FirstPassVisitor<'ast, 'src> {
    type ReturnType = Result<(), AluminaError>;

    fn visit_source_file(&mut self, node: Node<'src>) -> Self::ReturnType {
        parse_attributes!(self, node);
        self.visit_children_by_field(node, "body")
    }

    fn visit_mod_definition(&mut self, node: Node<'src>) -> Self::ReturnType {
        let attributes = parse_attributes!(self, node);
        let name = self.parse_name(node);
        let child_scope = self.scope.named_child(ScopeType::Module, name);

        self.add_item(
            node,
            name,
            NamedItem::new(NamedItemKind::Module(child_scope.clone()), attributes),
        )?;

        with_child_scope!(self, child_scope, {
            self.visit_children_by_field(node, "body")?;
        });

        Ok(())
    }

    fn visit_top_level_block(&mut self, node: Node<'src>) -> Self::ReturnType {
        let _ = parse_attributes!(self, node);

        if node.child_by_field(FieldKind::Attributes).is_none() {
            self.global_ctx.diag().add_warning(CodeError {
                kind: CodeErrorKind::TopLevelBlockWithoutAttributes,
                backtrace: vec![Marker::Span(Span::from_node(self.code.file_id(), node))],
            })
        }

        self.visit_children_by_field(node, "items")
    }

    fn visit_protocol_definition(&mut self, node: Node<'src>) -> Self::ReturnType {
        let item = self.ast.make_symbol();
        let attributes = parse_attributes!(self, node, item);

        let name = self.parse_name(node);
        let child_scope = self.scope.named_child(ScopeType::Protocol, name);

        self.add_item(
            node,
            name,
            NamedItem::new(
                NamedItemKind::Protocol(item, node, child_scope.clone()),
                attributes,
            ),
        )?;

        with_child_scope_container!(self, child_scope, {
            if let Some(f) = node.child_by_field(FieldKind::TypeArguments) {
                self.visit(f)?;
            }
            self.visit_children_by_field(node, "body")?;
        });

        Ok(())
    }

    fn visit_struct_definition(&mut self, node: Node<'src>) -> Self::ReturnType {
        let item = self.ast.make_symbol();
        let attributes = parse_attributes!(self, node, item);

        let name = self.parse_name(node);
        let child_scope = self.scope.named_child(ScopeType::StructLike, name);

        self.add_item(
            node,
            name,
            NamedItem::new(
                NamedItemKind::Type(item, node, child_scope.clone()),
                attributes,
            ),
        )?;

        with_child_scope!(self, child_scope, {
            if let Some(f) = node.child_by_field(FieldKind::TypeArguments) {
                self.visit(f)?;
            }
            self.visit_children_by_field(node, "body")?;
        });

        Ok(())
    }

    fn visit_impl_block(&mut self, node: Node<'src>) -> Self::ReturnType {
        let attributes = parse_attributes!(self, node);

        let name = self.parse_name(node);
        let child_scope = self.scope.named_child(ScopeType::Impl, name);

        self.add_item(
            node,
            name,
            NamedItem::new(NamedItemKind::Impl(node, child_scope.clone()), attributes),
        )?;

        with_child_scope_container!(self, child_scope, {
            if let Some(f) = node.child_by_field(FieldKind::TypeArguments) {
                self.visit(f)?;
            }
            self.visit_children_by_field(node, "body")?;
        });

        Ok(())
    }

    fn visit_enum_definition(&mut self, node: Node<'src>) -> Self::ReturnType {
        let item = self.ast.make_symbol();
        let attributes = parse_attributes!(self, node, item);

        let name = self.parse_name(node);
        let child_scope = self.scope.named_child(ScopeType::Enum, name);

        self.add_item(
            node,
            name,
            NamedItem::new(
                NamedItemKind::Type(item, node, child_scope.clone()),
                attributes,
            ),
        )?;

        with_child_scope!(self, child_scope, {
            self.enum_item = Some(item);
            self.visit_children_by_field(node, "body")?;
        });

        Ok(())
    }

    fn visit_enum_item(&mut self, node: Node<'src>) -> Self::ReturnType {
        let attributes = parse_attributes!(self, node);

        let name = self.parse_name(node);
        self.add_item(
            node,
            name,
            NamedItem::new(
                NamedItemKind::EnumMember(self.enum_item.unwrap(), self.ast.make_id(), node),
                attributes,
            ),
        )?;

        Ok(())
    }

    fn visit_struct_field(&mut self, node: Node<'src>) -> Self::ReturnType {
        let attributes = parse_attributes!(self, node);

        let name = self.parse_name(node);
        self.add_item(
            node,
            name,
            NamedItem::new(NamedItemKind::Field(node), attributes),
        )?;

        Ok(())
    }

    fn visit_function_definition(&mut self, node: Node<'src>) -> Self::ReturnType {
        let item = self.ast.make_symbol();
        let attributes = parse_attributes!(self, node, item);

        let name = self.parse_name(node);

        if let Some(path) = self.main_module_path.as_ref() {
            if self.global_ctx.cfg("test").is_some() {
                if attributes.contains(&Attribute::TestMain)
                    && self.main_candidate.replace(item).is_some()
                {
                    return Err(CodeErrorKind::MultipleMainFunctions)
                        .with_span_from(&self.scope, node);
                }
            } else if &self.scope.path() == path
                && name == "main"
                && !attributes.contains(&Attribute::Export)
                && !attributes
                    .iter()
                    .any(|a| matches!(a, Attribute::LinkName(..)))
                && self.main_candidate.replace(item).is_some()
            {
                return Err(CodeErrorKind::MultipleMainFunctions).with_span_from(&self.scope, node);
            }
        }

        let child_scope = self.scope.named_child(ScopeType::Function, name);

        self.add_item(
            node,
            name,
            NamedItem::new(
                if self.in_a_container {
                    NamedItemKind::Method(item, node, child_scope.clone())
                } else {
                    NamedItemKind::Function(item, node, child_scope.clone())
                },
                attributes,
            ),
        )?;

        with_child_scope!(self, child_scope, {
            if let Some(f) = node.child_by_field(FieldKind::TypeArguments) {
                self.visit(f)?;
            }
            self.visit_children_by_field(node, "parameters")?;
        });

        Ok(())
    }

    fn visit_type_definition(&mut self, node: Node<'src>) -> Self::ReturnType {
        let item = self.ast.make_symbol();
        let attributes = parse_attributes!(self, node, item);

        let name = self.parse_name(node);

        let child_scope = self.scope.named_child(ScopeType::Function, name);

        self.add_item(
            node,
            name,
            NamedItem::new(
                NamedItemKind::TypeDef(item, node, child_scope.clone()),
                attributes,
            ),
        )?;

        with_child_scope!(self, child_scope, {
            if let Some(f) = node.child_by_field(FieldKind::TypeArguments) {
                self.visit(f)?;
            }
        });

        Ok(())
    }

    fn visit_mixin(&mut self, node: Node<'src>) -> Self::ReturnType {
        let attributes = parse_attributes!(self, node);
        let child_scope = self.scope.anonymous_child(ScopeType::Function);

        self.add_unnamed_item(
            node,
            NamedItem::new(NamedItemKind::Mixin(node, child_scope.clone()), attributes),
        )?;

        with_child_scope!(self, child_scope, {
            if let Some(f) = node.child_by_field(FieldKind::TypeArguments) {
                self.visit(f)?;
            }
        });

        Ok(())
    }

    fn visit_static_declaration(&mut self, node: Node<'src>) -> Self::ReturnType {
        let item = self.ast.make_symbol();
        let attributes = parse_attributes!(self, node, item);

        let name = self.parse_name(node);
        let child_scope = self.scope.named_child(ScopeType::Function, name);

        self.add_item(
            node,
            name,
            NamedItem::new(
                NamedItemKind::Static(item, node, child_scope.clone()),
                attributes,
            ),
        )?;

        with_child_scope!(self, child_scope, {
            if let Some(f) = node.child_by_field(FieldKind::TypeArguments) {
                self.visit(f)?;
            }
        });

        Ok(())
    }

    fn visit_const_declaration(&mut self, node: Node<'src>) -> Self::ReturnType {
        let item = self.ast.make_symbol();
        let attributes = parse_attributes!(self, node, item);

        let name = self.parse_name(node);
        let child_scope = self.scope.named_child(ScopeType::Function, name);

        self.add_item(
            node,
            name,
            NamedItem::new(
                NamedItemKind::Const(item, node, child_scope.clone()),
                attributes,
            ),
        )?;

        with_child_scope!(self, child_scope, {
            if let Some(f) = node.child_by_field(FieldKind::TypeArguments) {
                self.visit(f)?;
            }
        });

        Ok(())
    }

    fn visit_generic_argument_list(&mut self, node: Node<'src>) -> Self::ReturnType {
        let mut cursor = node.walk();
        for argument in node.children_by_field(FieldKind::Argument, &mut cursor) {
            let name = self
                .code
                .node_text(argument.child_by_field(FieldKind::Placeholder).unwrap())
                .alloc_on(self.ast);
            self.add_item(
                node,
                name,
                NamedItem::new_default(NamedItemKind::Placeholder(self.ast.make_id(), argument)),
            )?;
        }

        Ok(())
    }

    fn visit_parameter(&mut self, node: Node<'src>) -> Self::ReturnType {
        let name = self.parse_name(node);

        self.add_item(
            node,
            name,
            NamedItem::new_default(NamedItemKind::Parameter(self.ast.make_id(), node)),
        )?;

        Ok(())
    }

    fn visit_macro_parameter(&mut self, node: Node<'src>) -> Self::ReturnType {
        let name = self.parse_name(node);

        self.add_item(
            node,
            name,
            NamedItem::new_default(NamedItemKind::MacroParameter(
                self.ast.make_id(),
                node.child_by_field(FieldKind::EtCetera).is_some(),
                Span::from_node(self.scope.file_id(), node),
            )),
        )?;

        Ok(())
    }

    fn visit_parameter_list(&mut self, node: Node<'src>) -> Self::ReturnType {
        self.visit_children_by_field(node, "parameter")
    }

    fn visit_macro_parameter_list(&mut self, node: Node<'src>) -> Self::ReturnType {
        self.visit_children_by_field(node, "parameter")
    }

    fn visit_use_declaration(&mut self, node: Node<'src>) -> Self::ReturnType {
        let attributes = parse_attributes!(self, node);

        let mut visitor =
            UseClauseVisitor::new(self.ast, self.scope.clone(), attributes, self.macro_ctx);
        visitor.visit(node.child_by_field(FieldKind::Argument).unwrap())?;

        Ok(())
    }

    fn visit_macro_definition(&mut self, node: Node<'src>) -> Self::ReturnType {
        let item = self.ast.make_symbol();
        let attributes = parse_attributes!(self, node, item);

        let name = self.parse_name(node);
        let child_scope = self.scope.named_child(ScopeType::Macro, name);

        self.add_item(
            node,
            name,
            NamedItem::new(
                NamedItemKind::Macro(item, node, child_scope.clone()),
                attributes,
            ),
        )?;

        with_child_scope!(self, child_scope, {
            self.visit_children_by_field(node, "parameters")?;
        });

        Ok(())
    }

    fn visit_doc_comment(&mut self, _node: tree_sitter::Node<'src>) -> Self::ReturnType {
        Ok(())
    }

    fn visit_file_doc_comment(&mut self, _node: tree_sitter::Node<'src>) -> Self::ReturnType {
        Ok(())
    }
}
