use std::fmt;
use std::rc::Rc;
use std::cell::{RefCell};
use std::collections::HashMap;
use super::position;
use super::token::{Token, LOWEST_PREC};
use super::scanner;
use super::errors;
use super::scope::*;
use super::ast::*;
use super::ast_objects::*;

pub struct Parser<'a> {
    objects: Objects,
    scanner: scanner::Scanner<'a>,
    errors: Rc<RefCell<errors::ErrorList>>,

    trace: bool,
    indent: isize,

    pos: position::Pos,
    token: Token,

    sync_pos: position::Pos,
    sync_count: isize,

    expr_level: isize,
    in_rhs: bool,

    pkg_scope: Option<ScopeIndex>,
    top_scope: Option<ScopeIndex>,
    unresolved: Vec<IdentIndex>,
    imports: Vec<SpecIndex>, //ImportSpec

    label_scope: Option<ScopeIndex>,
    target_stack: Vec<Vec<IdentIndex>>,
}

impl<'a> Parser<'a> {
    fn new(file: &'a mut position::File, src: &'a str, trace: bool) -> Parser<'a> {
        let err = Rc::new(RefCell::new(errors::ErrorList::new()));
        let s = scanner::Scanner::new(file, src, err.clone());
        Parser{
            objects: Objects::new(),
            scanner: s,
            errors: err,
            trace: trace,
            indent: 0,
            pos: 0,
            token: Token::ILLEGAL("".to_string()),
            sync_pos: 0,
            sync_count: 0,
            expr_level: 0,
            in_rhs: false,
            pkg_scope: None,
            top_scope: None,
            unresolved: vec![],
            imports: vec![],
            label_scope:None,
            target_stack: vec![],
        }
    }

    // ----------------------------------------------------------------------------
    // Scoping support

    fn open_scope(&mut self) {
        self.top_scope = Some(new_scope!(self, self.top_scope.take()));
    }

    fn close_scope(&mut self) {
        self.top_scope = scope!(self, self.top_scope.take().unwrap()).outer;
    }

    fn open_label_scope(&mut self) { 
        self.label_scope = 
            Some(new_scope!(self, self.label_scope.take()));
        self.target_stack.push(vec![]);
    }

    fn close_label_scope(&mut self) {
        let scope = scope!(self, *self.label_scope.as_ref().unwrap());
        match self.target_stack.pop() {
            Some(v) => {
                for i in v {
                    let ident = ident!(self, i);
                    if scope.look_up(&ident.name).is_none() {
                        let s = format!("label {} undefined", ident.name);
                        self.error_string(self.pos, s);
                    }
                }
            }
            _ => panic!("invalid target stack.")
        }
        self.label_scope = scope!(self, self.label_scope.take().unwrap()).outer;
    }

    fn declare(&mut self, decl: DeclObj, data: EntityData, kind: EntityKind,
        scope_ind: &ScopeIndex) {
        let mut names: Vec<IdentIndex> = vec![];
        let idents = match decl {
            DeclObj::Field(id) => &(field!(self, id).names),
            DeclObj::Spec(id) => { 
                match spec!(self, id) {
                    Spec::Value(vs) => &vs.names,
                    Spec::Type(ts) => {names.push(ts.name); &names},
                    Spec::Import(_) => &names,
                }},
            DeclObj::FuncDecl(i) => {
                let func_decl = fn_decl!(self, i);
                names.push(func_decl.name);
                &names
            }
            DeclObj::LabeledStmt(i) => {
                let lab_stmt = lab_stmt!(self, i);
                names.push(lab_stmt.label);
                &names
            }
            DeclObj::AssignStmt(_) => {
              panic!("unreachable");
            }
            DeclObj::NoDecl => &names,
        };
        for id in idents.iter() {
            let mut_ident = ident_mut!(self, *id);
            let entity = new_entity!(self, kind.clone(), 
                mut_ident.name.clone(), decl.clone(), data.clone());
            mut_ident.entity = IdentEntity::Entity(entity);
            let ident = ident!(self, *id);
            if ident.name != "_" {
                let scope = scope_mut!(self, *scope_ind);
                match scope.insert(ident.name.clone(), entity) {
                    Some(prev_decl) => {
                        let p =  entity!(self, prev_decl).pos(&self.objects);
                        let mut buf = String::new();
                        fmt::write(&mut buf, format_args!(
                            "{} redeclared in this block\n\tprevious declaration at {}",
                            ident.name, 
                            self.file().position(p))).unwrap();
                        self.error_string(ident.pos, buf);
                    },
                    _ => {},
                }
            }
        }
    }

    fn short_var_decl(&mut self, stmt: &Stmt) {
        // Go spec: A short variable declaration may redeclare variables
        // provided they were originally declared in the same block with
        // the same type, and at least one of the non-blank variables is new.
        let assign = if let Stmt::Assign(idx) = stmt {
            *idx.as_ref()
        } else {
            panic!("unreachable");
        };
        let list = &ass_stmt!(self, assign).lhs;
	    let mut n = 0; // number of new variables
        for expr in list {
            match expr { 
                Expr::Ident(id) => {
                    let ident = ident_mut!(self, *id.as_ref());
                    let entity = new_entity!(self, EntityKind::Var, 
                        ident.name.clone(), DeclObj::AssignStmt(assign), 
                        EntityData::NoData);
                    ident.entity = IdentEntity::Entity(entity);
                    if ident.name != "_" {
                        let top_scope = scope_mut!(self, self.top_scope.unwrap());
                        match top_scope.insert(ident.name.clone(), entity) {
                            Some(e) => { ident.entity = IdentEntity::Entity(e); },
                            None => { n += 1; },
                        }
                    }
                },
                _ => {
                    self.error_expected(expr.pos(&self.objects), 
                        "identifier on left side of :=");
                },
            }
        }
        if n == 0 {
            self.error(list[0].pos(&self.objects), 
                "no new variables on left side of :=")
        }
    }

    // If x is an identifier, tryResolve attempts to resolve x by looking up
    // the object it denotes. If no object is found and collectUnresolved is
    // set, x is marked as unresolved and collected in the list of unresolved
    // identifiers.
    fn try_resolve(&mut self, x: &Expr, collect_unresolved: bool) {
        if let Expr::Ident(i) = x {
            let ident = ident_mut!(self, *i.as_ref());
            assert!(ident.entity.is_none(), 
                "identifier already declared or resolved");
            if ident.name == "_" {
                return;
            }
            // try to resolve the identifier
            let mut s = self.top_scope;
            loop {
                match s {
                    Some(sidx) => {
                        let scope = scope!(self, sidx);
                        if let Some(entity) = scope.look_up(&ident.name) {
                            ident.entity = IdentEntity::Entity(*entity);
                            return;
                        }
                        s = scope.outer;
                    },
                    None => {break;},
                }
            }
            // all local scopes are known, so any unresolved identifier
            // must be found either in the file scope, package scope
            // (perhaps in another file), or universe scope --- collect
            // them so that they can be resolved later
            if collect_unresolved {
                ident.entity = IdentEntity::Sentinel;
                self.unresolved.push(*i.as_ref());
            }
        }
    }

    fn resolve(&mut self, x: &Expr) {
        self.try_resolve(x, true)
    }

    // ----------------------------------------------------------------------------
    // Parsing support

    fn file_mut(&mut self) -> &mut position::File {
        self.scanner.file_mut()
    }

    fn file(&self) -> &position::File {
        self.scanner.file()
    }

    fn print_trace(&self, msg: &str) {
        let f = self.file();
        let p = f.position(self.pos);
        let mut buf = String::new();
        fmt::write(&mut buf, format_args!("{:5o}:{:3o}:", p.line, p.column)).unwrap();
        for _ in 0..self.indent {
            buf.push_str("..");
        }
        print!("{}{}\n", buf, msg);
    }

    fn trace_begin(&mut self, msg: &str) {
        if self.trace {
            let mut trace_str = msg.to_string();
            trace_str.push('(');
            self.print_trace(&trace_str);
            self.indent += 1;
        }
    }

    fn trace_end(&mut self) {
        if self.trace {
            self.indent -= 1;
            self.print_trace(")");
        }
    }

    fn next(&mut self) {
        // Print previous token
        if self.pos > 0 {
            self.print_trace(&format!("next: {}", self.token));
        }
        // Get next token and skip comments
        let mut token: Token;
        loop {
            token = self.scanner.scan();
            match token {
                Token::COMMENT(_) => { // Skip comment
                    self.print_trace(&format!("{}", self.token));
                },
                _ => { break; },
            }
        }
        self.token = token;
        self.pos = self.scanner.pos();
    }

    fn error(&self, pos: position::Pos, s: &str) {
        self.error_string(pos, s.to_string());
    }

    fn error_string(&self, pos: position::Pos, msg: String) {
        let p = self.file().position(pos);
        self.errors.borrow_mut().add(p, msg);
    }

    fn error_expected(&self, pos: position::Pos, msg: &str) {
        let mut mstr = "expected ".to_string();
        mstr.push_str(msg);
        if pos == self.pos {
            match self.token {
                Token::SEMICOLON(real) => if !real {
                    mstr.push_str(", found newline");
                },
                _ => {
                    mstr.push_str(", found ");
                    mstr.push_str(self.token.text());
                }
            }
        }
        self.error_string(pos, mstr);
    }

    fn expect(&mut self, token: &Token) -> position::Pos {
        let pos = self.pos;
        if self.token != *token {
            self.error_expected(pos, &format!("'{}'", token));
        }
        self.next();
        pos
    }

    // https://github.com/golang/go/issues/3008
    // Same as expect but with better error message for certain cases
    fn expect_closing(&mut self, token: &Token, context: &str) -> position::Pos {
        if let Token::SEMICOLON(real) = token {
            if !real {
                let msg = format!("missing ',' before newline in {}", context);
                self.error_string(self.pos, msg);
                self.next();
            }
        }
        self.expect(token)
    }

    fn expect_semi(&mut self) {
        // semicolon is optional before a closing ')' or '}'
        match self.token {
            Token::RPAREN | Token::RBRACE => {},
            Token::SEMICOLON(_) => { self.next(); },
            _ => {
                if let Token::COMMA = self.token {
                    // permit a ',' instead of a ';' but complain
                    self.error_expected(self.pos, "';'");
                    self.next();
                }
                self.error_expected(self.pos, "';'");
                self.advance(Token::is_stmt_start);
            }
        }
    }

    fn at_comma(&self, context: &str, follow: &Token) -> bool {
        if let Token::COMMA = self.token {
            true
        } else if self.token == *follow {
            let mut msg =  "missing ','".to_string();
            if let Token::SEMICOLON(real) = self.token {
                if !real {msg.push_str(" before newline");}
            }
            msg = format!("{} in {}", msg, context);
            self.error_string(self.pos, msg);
            true
        } else {
            false
        }
    }

    // advance consumes tokens until the current token p.tok
    // is in the 'to' set, or token.EOF. For error recovery.
    fn advance(&mut self, to: fn(&Token) -> bool) {
        while self.token != Token::EOF {
            self.next();
            if to(&self.token) {
                // Return only if parser made some progress since last
                // sync or if it has not reached 10 advance calls without
                // progress. Otherwise consume at least one token to
                // avoid an endless parser loop (it is possible that
                // both parseOperand and parseStmt call advance and
                // correctly do not advance, thus the need for the
                // invocation limit p.syncCnt).
                if self.pos == self.sync_pos && self.sync_count < 10 {
                    self.sync_count += 1;
                    break;
                }
                if self.pos > self.sync_pos {
                    self.sync_pos = self.pos;
                    self.sync_count = 0;
                    break;
                }
                // Reaching here indicates a parser bug, likely an
                // incorrect token list in this function, but it only
                // leads to skipping of possibly correct code if a
                // previous error is present, and thus is preferred
                // over a non-terminating parse.
            }
        }
    }

    // safe_pos returns a valid file position for a given position: If pos
    // is valid to begin with, safe_pos returns pos. If pos is out-of-range,
    // safe_pos returns the EOF position.
    //
    // This is hack to work around "artificial" end positions in the AST which
    // are computed by adding 1 to (presumably valid) token positions. If the
    // token positions are invalid due to parse errors, the resulting end position
    // may be past the file's EOF position, which would lead to panics if used
    // later on.
    fn safe_pos(&self, pos: position::Pos) -> position::Pos {
        let max = self.file().base() + self.file().size(); 
        if pos > max { max } else { pos }
    }

    // ----------------------------------------------------------------------------
    // Identifiers

    fn parse_ident(&mut self) -> IdentIndex {
        let pos = self.pos;
        let mut name = "_".to_string();
        if let Token::IDENT(lit) = self.token.clone() {
            name = lit;
            self.next();
        } else {
            self.expect(&Token::IDENT("".to_string()));
        }
        self.objects.idents.insert(Ident{ pos: pos, name: name,
            entity: IdentEntity::NoEntity})
    }

    fn parse_ident_list(&mut self) -> Vec<IdentIndex> {
        self.trace_begin("IdentList");
        
        let mut list = vec![self.parse_ident()];
        while self.token == Token::COMMA {
            self.next();
            list.push(self.parse_ident());
        }
       
        self.trace_end();
        list
    }

    // ----------------------------------------------------------------------------
    // Common productions
    fn parse_expr_list(&mut self, lhs: bool) -> Vec<Expr> {
        self.trace_begin("ExpressionList");

        let expr = self.parse_expr(lhs);
        let mut list = vec![self.check_expr(expr)];
        while self.token == Token::COMMA {
            self.next();
            let expr = self.parse_expr(lhs);
            list.push(self.check_expr(expr));
        }

        self.trace_end();
        list
    }

    fn parse_lhs_list(&mut self) -> Vec<Expr> {
        let bak = self.in_rhs;
        self.in_rhs = false;
        let list = self.parse_expr_list(true);
        match self.token {
            // lhs of a short variable declaration
            // but doesn't enter scope until later:
            // caller must call self.short_var_decl(list)
            // at appropriate time.
            Token::DEFINE => {},
            // lhs of a label declaration or a communication clause of a select
            // statement (parse_lhs_list is not called when parsing the case clause
            // of a switch statement):
            // - labels are declared by the caller of parse_lhs_list
            // - for communication clauses, if there is a stand-alone identifier
            //   followed by a colon, we have a syntax error; there is no need
            //   to resolve the identifier in that case
            Token::COLON => {},
            _ => {
                // identifiers must be declared elsewhere
                for x in list.iter() {
                    self.resolve(x);
                }
            }
        }
        self.in_rhs = bak;
        list
    }

    fn parse_rhs_list(&mut self) -> Vec<Expr> {
        let bak = self.in_rhs;
        self.in_rhs = true;
        let list = self.parse_expr_list(false);
        self.in_rhs = bak;
        list
    }

    // ----------------------------------------------------------------------------
    // Types
    fn parse_type(&mut self) -> Expr {
        self.trace_begin("Type");

        let typ = self.try_type();
        let ret = if typ.is_none() {
            let pos = self.pos;
            self.error_expected(pos, "type");
            self.next();
            Expr::new_bad(pos, self.pos)
        } else {
            typ.unwrap()
        };
       
        self.trace_end();
        ret
    }
    
    // If the result is an identifier, it is not resolved.
    fn parse_type_name(&mut self) -> Expr {
        self.trace_begin("TypeName");

        let ident = self.parse_ident();
        let x_ident = Expr::Ident(Box::new(ident));
        // don't resolve ident yet - it may be a parameter or field name
        let ret = if let Token::PERIOD = self.token {
            // ident is a package name
            self.next();
            self.resolve(&x_ident);
            let sel = self.parse_ident();
            Expr::new_selector(x_ident, sel)
        } else {
            x_ident
        };

        self.trace_end();
        ret
    }

    fn parse_array_type(&mut self) -> Expr {
        self.trace_begin("ArrayType");

        let lpos = self.expect(&Token::LBRACK);
        self.expr_level += 1;
        let len = match self.token {
            // always permit ellipsis for more fault-tolerant parsing
            Token::ELLIPSIS => {
                Some(Expr::new_ellipsis(self.pos, None))
            },
            _ if self.token != Token::RBRACK => {
                Some(self.parse_rhs())
            },
            _ => None,
        };
        self.expr_level -= 1;
        self.expect(&Token::RBRACK);
        let elt = self.parse_type();

        self.trace_end();
        Expr::Array(Box::new(ArrayType{
            l_brack: lpos, len: len, elt: elt}))
    }

    fn make_ident_list(&mut self, exprs: &mut Vec<Expr>) -> Vec<IdentIndex> {
        exprs.iter().map(|x| {
            match x {
                Expr::Ident(ident) => *ident.as_ref(),
                _ => {
                    let pos = x.pos(&self.objects);
                    if let Expr::Bad(_) = x {
                        // only report error if it's a new one
                        self.error_expected(pos, "identifier")
                    }
                    new_ident!(self, pos, "_".to_string(), IdentEntity::NoEntity)
                }
            }
        }).collect()
    }

    
    fn parse_field_decl(&mut self, scope: ScopeIndex) -> FieldIndex {
        self.trace_begin("FieldDecl");

        // 1st FieldDecl
	    // A type name used as an anonymous field looks like a field identifier.
        let mut list = vec![];
        loop {
            list.push(self.parse_var_type(false));
            if let Token::COMMA = self.token {
                break;
            }
            self.next();
        }

        let mut idents = vec![];
        let typ = match self.try_var_type(false) {
            Some(t) => {
                idents = self.make_ident_list(&mut list);
                t
            }
            // ["*"] TypeName (AnonymousField)
            None => { 
                let first = &list[0]; // we always have at least one element
                if list.len() > 1 {
                    self.error_expected(self.pos, "type");
                    Expr::new_bad(self.pos, self.pos)
                } else if !Parser::is_type_name(Parser::deref(first)) {
                    self.error_expected(self.pos, "anonymous field");
                    Expr::new_bad(
                        first.pos(&self.objects),
                        self.safe_pos(first.end(&self.objects)))
                } else {
                    list.into_iter().nth(0).unwrap()
                }
            }
        };

        // Tag
        let token = self.token.clone();
        let tag = if let Token::STRING(_) = token {
            self.next();
            Some(Expr::new_basic_lit(self.pos, self.token.clone()))
        } else {
            None
        };

        self.expect_semi();

        // have to clone to fix ownership issue.
        let field = new_field!(self, idents, typ.clone_ident(), tag);
        self.declare(DeclObj::Field(field), EntityData::NoData,
            EntityKind::Var, &scope);
        self.resolve(&typ);

        self.trace_end();
        field
    }

    fn parse_struct_type(&mut self) -> Expr {
        self.trace_begin("FieldDecl");

        let stru = self.expect(&Token::STRUCT);
        let lbrace = self.expect(&Token::LBRACE);
        let scope = new_scope!(self, None);
        let mut list = vec![];
        loop {
            match &self.token {
                Token::IDENT(_) | Token::MUL | Token::LPAREN => {
                    list.push(self.parse_field_decl(scope));
                }
                _ => {break;}
            } 
        }
        let rbrace = self.expect(&Token::RBRACE);

        self.trace_end();
        Expr::Struct(Box::new(StructType{
            struct_pos: stru,
            fields: FieldList::new(Some(lbrace), list, Some(rbrace)),
            incomplete: false,
        }))
    }

    fn parse_pointer_type(&mut self) -> Expr {
        self.trace_begin("PointerType");

        let star = self.expect(&Token::MUL);
        let base = self.parse_type();

        self.trace_end();
        Expr::Star(Box::new(StarExpr{star: star, expr: base}))
    }

    // If the result is an identifier, it is not resolved.
    fn try_var_type(&mut self, is_param: bool) -> Option<Expr> {
        if is_param {
            if let Token::ELLIPSIS = self.token {
                let pos = self.pos;
                self.next();
                let typ = if let Some(t) = self.try_ident_or_type() {
                    self.resolve(&t);
                    t
                    
                } else {
                    self.error(pos, "'...' parameter is missing type");
                    Expr::new_bad(pos, self.pos)
                };
                return Some(Expr::new_ellipsis(pos, Some(typ)));
            }
        }
        self.try_ident_or_type()
    }

    fn parse_var_type(&mut self, is_param: bool) -> Expr {
        match self.try_var_type(is_param) {
            Some(typ) => typ,
            None => {
                let pos = self.pos;
                self.error_expected(pos, "type");
                self.next();
                Expr::new_bad(pos, self.pos)
            },
        }
    }

    fn parse_parameter_list(&mut self, scope: ScopeIndex,
        ellipsis_ok: bool) -> Vec<FieldIndex> {
        self.trace_begin("ParameterList");

        // 1st ParameterDecl
	    // A list of identifiers looks like a list of type names.
        let mut list = vec![];
        loop {
            list.push(self.parse_var_type(ellipsis_ok));
            if let Token::COMMA = &self.token {
                break;
            }
            self.next();
            if let Token::RPAREN = &self.token {
                break;
            }
        }

        let mut params = vec![];
        let typ = self.try_var_type(ellipsis_ok);
        if let Some(t) = typ {
            // IdentifierList Type
            let idents = self.make_ident_list(&mut list);
            let field = new_field!(self, idents, t.clone_ident(), None);
            params.push(field);
            // Go spec: The scope of an identifier denoting a function
			// parameter or result variable is the function body.
			self.declare(DeclObj::Field(field), EntityData::NoData,
                EntityKind::Var, &scope);
            self.resolve(&t);
            if !self.at_comma("parameter list", &Token::RPAREN) {
                self.trace_end();
                return params;
            }
            self.next();
            loop {
                let idents = self.parse_ident_list();
                let t = self.parse_var_type(ellipsis_ok);
                let field = new_field!(self, idents, t.clone_ident(), None);
                // warning: copy paste
                params.push(field);
                // Go spec: The scope of an identifier denoting a function
                // parameter or result variable is the function body.
                self.declare(DeclObj::Field(field), EntityData::NoData,
                    EntityKind::Var, &scope);
                self.resolve(&t);
                if !self.at_comma("parameter list", &Token::RPAREN) {
                    break;
                }
                self.next();
            }
        } else {
            // Type { "," Type } (anonymous parameters)
            for typ in list {
                self.resolve(&typ);
                params.push(new_field!(self, vec![], typ, None));
            }
        }
        self.trace_end();
        params
    }

    fn parse_parameters(&mut self, scope: ScopeIndex,
        ellipsis_ok: bool) -> FieldList {
        self.trace_begin("Parameters");

        let mut params = vec![];
        let lparen = Some(self.expect(&Token::LPAREN));
        if self.token != Token::RPAREN {
            params = self.parse_parameter_list(scope, ellipsis_ok);
        }
        let rparen = Some(self.expect(&Token::RPAREN));

        self.trace_end();
        FieldList::new(lparen, params, rparen)
    }

    fn parse_result(&mut self, scope: ScopeIndex) -> FieldList {
        self.trace_begin("Result");

        let ret = if self.token == Token::LPAREN {
            self.parse_parameters(scope, false)
        } else {
            if let Some(t) = self.try_type() {
                let field = new_field!(self, vec![], t, None);
                FieldList::new(None, vec![field], None)
            } else {
                FieldList::new(None, vec![], None)
            }
        };

        self.trace_end();
        ret
    }

    fn parse_signature(&mut self, scope: ScopeIndex) -> (FieldList, FieldList) {
        self.trace_begin("Result");

        let params = self.parse_parameters(scope, true);
        let results = self.parse_result(scope);

        self.trace_end();   
        (params, results)
    }

    fn parse_func_type(&mut self) -> (FuncType, ScopeIndex) {
        self.trace_begin("FuncType");

        let pos = self.expect(&Token::FUNC);
        let scope = new_scope!(self, self.top_scope);
        let (params, results) = self.parse_signature(scope);

        self.trace_end();
        (FuncType::new(Some(pos), params, Some(results)), scope)
    }

    // method spec in interface
    fn parse_method_spec(&mut self, scope: ScopeIndex) -> FieldIndex {
        self.trace_begin("MethodSpec");

        let mut idents = vec![];
        let mut typ = self.parse_type_name();
        let ident = typ.unwrap_ident().clone();
        if let Token::LPAREN = self.token {
            idents = vec![ident];
            let scope = new_scope!(self, self.top_scope);
            let (params, results) = self.parse_signature(scope);
            typ = Expr::box_func_type(FuncType::new(None, params, Some(results)));
        } else {
            // embedded interface
            self.resolve(&typ);
        }
        self.expect_semi();
        let field = new_field!(self, idents, typ, None);
        self.declare(DeclObj::Field(field), EntityData::NoData, EntityKind::Fun, &scope);

        self.trace_end();
        field
    }

    fn parse_interface_type(&mut self) -> InterfaceType {
        self.trace_begin("InterfaceType");

        let pos = self.expect(&Token::INTERFACE);
        let lbrace = self.expect(&Token::LBRACE);
        let scope = new_scope!(self, None);
        let mut list = vec![];
        loop {
            if let Token::IDENT(_) = self.token {} else {break;}
            list.push(self.parse_method_spec(scope));
        }
        let rbrace = self.expect(&Token::RBRACE);

        self.trace_end();
        InterfaceType{
            interface: pos,
            methods: FieldList{
                openning: Some(lbrace),
                list: list,
                closing: Some(rbrace),
            },
            incomplete: false,
        }
    }

    fn parse_map_type(&mut self) -> MapType {
        self.trace_begin("MapType");

        let pos = self.expect(&Token::MAP);
        self.expect(&Token::LBRACK);
        let key = self.parse_type();
        self.expect(&Token::RBRACK);
        let val = self.parse_type();

        self.trace_end();
        MapType{map: pos, key: key, val: val}
    }

    fn parse_chan_type(&mut self) -> ChanType {
        self.trace_begin("ChanType");

        let pos = self.pos;
        let arrow_pos: position::Pos;
        let dir: ChanDir;
        if let Token::CHAN = self.token {
            self.next();
            if let Token::ARROW = self.token {
                arrow_pos = self.pos;
                self.next();
                dir = ChanDir::Send;
            } else {
                arrow_pos = 0;
                dir = ChanDir::SendRecv;
            }
        } else {
            arrow_pos = self.expect(&Token::ARROW);
            self.expect(&Token::CHAN);
            dir = ChanDir::Recv;
        }
        let val = self.parse_type();

        self.trace_end();
        ChanType{begin: pos, arrow: arrow_pos, dir: dir, val: val}
    }

    // Returns a ident or a type
    // If the result is an identifier, it is not resolved.
    fn try_ident_or_type(&mut self) -> Option<Expr> {
        match self.token {
            Token::IDENT(_) => Some(self.parse_type_name()),
            Token::LBRACK => Some(self.parse_array_type()),
            Token::STRUCT => Some(self.parse_struct_type()),
            Token::MUL => Some(self.parse_pointer_type()),
            Token::FUNC => {
                let (typ, _) = self.parse_func_type();
                Some(Expr::box_func_type(typ))
            },
            Token::INTERFACE => Some(Expr::Interface(Box::new(
                self.parse_interface_type()))),
            Token::MAP => Some(Expr::Map(Box::new(
                self.parse_map_type()))),
            Token::CHAN | Token::ARROW => Some(Expr::Chan(Box::new(
                self.parse_chan_type()))),
            Token::LPAREN => {
                let lparen = self.pos;
                self.next();
                let typ = self.parse_type();
                let rparen = self.expect(&Token::RPAREN);
                Some(Expr::Paren(Box::new(ParenExpr{
                    l_paren: lparen, expr: typ, r_paren: rparen})))
            }
            _ => None
        }
    }

    fn try_type(&mut self) -> Option<Expr> {
        if let Some(typ) = self.try_ident_or_type() {
            self.resolve(&typ);
            Some(typ)
        } else {
            None
        }
    }

    // ----------------------------------------------------------------------------
    // Blocks

    fn parse_stmt_list(&mut self) -> Vec<Stmt> {
        self.trace_begin("Body");

        let mut list = vec![];
        loop {
            match self.token {
                Token::CASE | Token::DEFAULT | Token::RBRACE |
                Token::EOF => {break;},
                _ => {},
            };
            list.push(self.parse_stmt());
        }

        self.trace_end();  
        list    
    }
    
    fn parse_body(&mut self, scope: ScopeIndex) -> BlockStmt {
        self.trace_begin("Body");

        let lbrace = self.expect(&Token::LBRACE);
        self.top_scope = Some(scope); // open function scope
        self.open_label_scope();
        let list = self.parse_stmt_list();
        self.close_label_scope();
        self.close_scope();
        let rbrace = self.expect(&Token::RBRACE);

        self.trace_end();
        BlockStmt::new(lbrace, list, rbrace)
    }

    fn parse_block_stmt(&mut self) -> Stmt {
        self.trace_begin("BlockStmt");

        let lbrace = self.expect(&Token::LBRACE);
        self.open_scope();
        let list = self.parse_stmt_list();
        self.close_scope();
        let rbrace = self.expect(&Token::RBRACE);

        self.trace_end();
        Stmt::box_block(BlockStmt::new(lbrace, list, rbrace))
    }
    
    // ----------------------------------------------------------------------------
    // Expressions

    fn parse_func_type_or_lit(&mut self) -> Expr {
        self.trace_begin("BlockStmt");

        let (typ, scope) = self.parse_func_type();
        let ret = if self.token != Token::LBRACE {
            Expr::box_func_type(typ)
        } else {
            self.expr_level += 1;
            let body = self.parse_body(scope);
            self.expr_level -= 1;
            Expr::FuncLit(Box::new(FuncLit{typ: typ, body: body}))
        }; 
 
        self.trace_end(); 
        ret
    }

    // parseOperand may return an expression or a raw type (incl. array
    // types of the form [...]T. Callers must verify the result.
    // If lhs is set and the result is an identifier, it is not resolved.
    fn parse_operand(&mut self, lhs: bool) -> Expr {
        self.trace_begin("Operand");

        let ret = match self.token {
            Token::IDENT(_) => {
                let x = Expr::Ident(Box::new(self.parse_ident()));
                if !lhs {self.resolve(&x);}
                x
            },
            Token::INT(_) | Token::FLOAT(_) | Token::IMAG(_) |
            Token::CHAR(_) | Token::STRING(_) => {
                let x = Expr::new_basic_lit(self.pos, self.token.clone());
                self.next();
                x
            },
            Token::LPAREN => {
                let lparen = self.pos;
                self.next();
                self.expr_level += 1;
                // types may be parenthesized: (some type)
                let x = self.parse_rhs_or_type(); 
                self.expr_level -= 1;
                let rparen = self.expect(&Token::RPAREN);
                Expr::Paren(Box::new(ParenExpr{
                    l_paren: lparen, expr: x, r_paren: rparen}))
            },
            Token::FUNC => self.parse_func_type_or_lit(),
            _ => {
                if let Some(typ) = self.try_ident_or_type() {
                    if let Expr::Ident(_) = typ {
                        // unreachable but would work, so don't panic
                        assert!(false, "should only get idents here");
                    }
                    typ
                } else {
                    let pos = self.pos;
                    self.error_expected(pos, "operand");
                    self.advance(Token::is_stmt_start);
                    Expr::new_bad(pos, self.pos)
                }
            }
        };

        self.trace_end();
        ret
    }

    fn parse_selector(&mut self, x: Expr) -> Expr {
        self.trace_begin("Selector");
        let sel = self.parse_ident();
        self.trace_end();
        Expr::Selector(Box::new(SelectorExpr{
            expr: x, sel: sel}))
    }

    fn parse_type_assertion(&mut self, x: Expr) -> Expr {
        self.trace_begin("TypeAssertion");

        let lparen = self.expect(&Token::LPAREN);
        let typ = if self.token == Token::TYPE {
            // type switch: typ == nil, i.e.: x.(type)
            self.next();
            None
        } else {
            Some(self.parse_type())
        };
        let rparen = self.expect(&Token::RPAREN);
        
        self.trace_end();
        Expr::TypeAssert(Box::new(TypeAssertExpr{
            expr: x, l_paren: lparen, typ: typ, r_paren: rparen}))
    }

    fn parse_index_or_slice(&mut self, x: Expr) -> Expr {
        self.trace_begin("IndexOrSlice");

        const N: usize = 3; // change the 3 to 2 to disable 3-index slices
        let lbrack = self.expect(&Token::LBRACK);
        self.expr_level += 1;
        let mut indices = vec![None, None, None];
        let mut colons = vec![0, 0, 0];
        let mut ncolons = 0;
        if self.token != Token::COLON {
            indices[0] = Some(self.parse_rhs());
        }
        while self.token == Token::COLON && ncolons < N  {
            colons[ncolons] = self.pos;
            ncolons += 1;
            self.next();
            match self.token {
                Token::COLON | Token::RBRACE | Token::EOF => {},
                _ => {indices[ncolons] = Some(self.parse_rhs())},
            }
        }
        self.expr_level -= 1;
        let rbrack = self.expect(&Token::RBRACK);
        let ret = if ncolons > 0 {
            let slice3 = ncolons == 2;
            if slice3 { // 3-index slices
                if indices[1].is_none() {
                    self.error(colons[0], "2nd index required in 3-index slice");
                    indices[1] = Some(Expr::new_bad(colons[0] + 1, colons[1]))
                }
                if indices[2].is_none() {
                    self.error(colons[1], "3rd index required in 3-index slice");
                    indices[2] = Some(Expr::new_bad(colons[1] + 1, colons[2]))
                }
            }
            let mut iter = indices.into_iter();
            Expr::Slice(Box::new(SliceExpr{
                expr: x,
                l_brack: lbrack,
                low: iter.next().unwrap(), // unwrap the first of two Option
                high: iter.next().unwrap(),
                max: iter.next().unwrap(),
                slice3: slice3,
                r_brack: rbrack,
            }))
        } else {
            // the logic here differs from the original go code
            if indices[0].is_none() {
                self.error(lbrack, "expression for index value required");
                indices[0] = Some(Expr::new_bad(lbrack + 1, rbrack));
            }
            let index = indices.into_iter().nth(0).unwrap().unwrap();
            Expr::Index(Box::new(IndexExpr{
                expr: x, l_brack: lbrack, index: index, r_brack: rbrack}))
        };

        self.trace_end();
        ret
    }

    fn parse_call_or_conversion(&mut self, func: Expr) -> Expr {
        self.trace_begin("CallOrConversion");

        let lparen = self.expect(&Token::LPAREN);
        self.expr_level += 1;
        let mut list = vec![];
        let mut ellipsis: Option<position::Pos> = None;
        while self.token != Token::RPAREN && self.token != Token::EOF && 
            ellipsis.is_some() {
            //// builtins may expect a type: make(some_type)
            list.push(self.parse_rhs_or_type());
            if self.token == Token::ELLIPSIS {
                ellipsis = Some(self.pos);
                self.next();
            }
            if !self.at_comma("argument list", &Token::RPAREN) {
                break;
            }
            self.next();
        }
        self.expr_level -= 1;
        let rparen = self.expect_closing(&Token::RPAREN, "argument list");

        self.trace_end();
        Expr::Call(Box::new(CallExpr{
            func: func, l_paren: lparen, args: list, ellipsis: ellipsis, r_paren: rparen}))
    }

    fn parse_value(&mut self, key_ok: bool) -> Expr {
        self.trace_begin("Value");

        let ret = if self.token == Token::LBRACE {
            self.parse_literal_value(None)
        } else {
            // Because the parser doesn't know the composite literal type, it cannot
            // know if a key that's an identifier is a struct field name or a name
            // denoting a value. The former is not resolved by the parser or the
            // resolver.
            //
            // Instead, _try_ to resolve such a key if possible. If it resolves,
            // it a) has correctly resolved, or b) incorrectly resolved because
            // the key is a struct field with a name matching another identifier.
            // In the former case we are done, and in the latter case we don't
            // care because the type checker will do a separate field lookup.
            //
            // If the key does not resolve, it a) must be defined at the top
            // level in another file of the same package, the universe scope, or be
            // undeclared; or b) it is a struct field. In the former case, the type
            // checker can do a top-level lookup, and in the latter case it will do
            // a separate field lookup.
            let x0 = self.parse_expr(key_ok);
            let x = self.check_expr(x0);
            if key_ok {
                if self.token == Token::COLON {
                    // Try to resolve the key but don't collect it
                    // as unresolved identifier if it fails so that
                    // we don't get (possibly false) errors about
                    // undeclared names.
                    self.try_resolve(&x, false)
                } else {
                    // not a key
                    self.resolve(&x)
                }
            }
            x
        };

        self.trace_end();  
        ret
    }

    fn parse_element(&mut self) -> Expr {
        self.trace_begin("Element");

        let x = self.parse_value(true);
        let ret = if self.token == Token::COLON {
            let colon = self.pos;
            self.next();
            Expr::KeyValue(Box::new(KeyValueExpr{
                key: x, colon: colon, val: self.parse_value(false) }))
        } else {
            x
        };

        self.trace_end(); 
        ret
    }

    fn parse_element_list(&mut self) -> Vec<Expr> {
        self.trace_begin("ElementList");

        let mut list = vec![];
        while self.token != Token::RBRACE && self.token != Token::EOF {
            list.push(self.parse_element());
            if !self.at_comma("composite literal", &Token::RBRACE) {
                break;
            }
            self.next();
        }

        self.trace_end();
        list
    }

    fn parse_literal_value(&mut self, typ: Option<Expr>) -> Expr {
        self.trace_begin("LiteralValue");

        let lbrace = self.expect(&Token::LBRACE);
        self.expr_level += 1;
        let elts = if self.token != Token::RBRACE {
            self.parse_element_list()
        } else {vec![]};
        self.expr_level -= 1;
        let rbrace = self.expect_closing(&Token::RBRACE, "composite literal");

        self.trace_end();
        Expr::CompositeLit(Box::new(CompositeLit{
            typ: typ, l_brace: lbrace, elts: elts, r_brace: rbrace, incomplete: false}))
    }

    // checkExpr checks that x is an expression (and not a type).
    fn check_expr(&self, x: Expr) -> Expr {
        match x {
            Expr::Bad(_) => x,
            Expr::Ident(_) => x,
            Expr::BasicLit(_) => x,
            Expr::FuncLit(_) => x,
            Expr::CompositeLit(_) => x,
            Expr::Paren(_) => { panic!("unreachable"); },
            Expr::Selector(_) => x,
            Expr::Index(_) => x,
            Expr::Slice(_) => x,
            // If t.Type == nil we have a type assertion of the form
            // y.(type), which is only allowed in type switch expressions.
            // It's hard to exclude those but for the case where we are in
            // a type switch. Instead be lenient and test this in the type
            // checker.
            Expr::TypeAssert(_) => x,
            Expr::Call(_) => x,
            Expr::Star(_) => x,
            Expr::Unary(_) => x,
            Expr::Binary(_) => x,
            _ => {
                self.error_expected(self.pos, "expression");
                Expr::new_bad(
                    x.pos(&self.objects), 
                    self.safe_pos(x.end(&self.objects)))
            }
        }
    }

    // isTypeName reports whether x is a (qualified) TypeName.
    fn is_type_name(x: &Expr) -> bool {
        match x {
            Expr::Bad(_) | Expr::Ident(_) => true,
            Expr::Selector(s) => {
                if let Expr::Ident(_) = s.expr {true} else {false}
            },
            _ => false
        }
    }

    // isLiteralType reports whether x is a legal composite literal type.
    fn is_literal_type(x: &Expr) -> bool {
        match x {
            Expr::Bad(_) | Expr::Ident(_)  | Expr::Array(_) |
            Expr::Struct(_) | Expr::Map(_) => true,
            Expr::Selector(s) => {
                if let Expr::Ident(_) = s.expr {true} else {false}
            },
            _ => false
        }
    }

    fn deref(x: &Expr) -> &Expr {
        if let Expr::Star(s) = x {&s.expr} else {x}
    }

    fn unparen(x: &Expr) -> &Expr {
        if let Expr::Paren(p) = x {Parser::unparen(&p.expr)} else {x}
    }

    // checkExprOrType checks that x is an expression or a type
    // (and not a raw type such as [...]T).
    fn check_expr_or_type(&self, x: Expr) -> Expr {
        let unparenx = Parser::unparen(&x);
        match unparenx {
            Expr::Paren(_) => {panic!("unreachable")},
            Expr::Array(array) => {
                if let Some(ellipsis) = &array.len {
                    self.error(ellipsis.pos(&self.objects), 
                        "expected array length, found '...'");
			        return Expr::new_bad(unparenx.pos(&self.objects),
                        self.safe_pos(unparenx.end(&self.objects))); 
                }
            },
            _ => {},
        }
        return x;
    }

    fn parse_primary_expr(&mut self, mut lhs: bool) -> Expr {
        self.trace_begin("PrimaryExpr");

        let mut x = self.parse_operand(lhs);
        loop {
            match self.token {
                Token::PERIOD => {
                    self.next();
                    if lhs {
                        self.resolve(&x);
                    }
                    match self.token {
                        Token::IDENT(_) => {
                            x = self.parse_selector(self.check_expr_or_type(x));
                        }
                        Token::LPAREN => {
                            x = self.parse_type_assertion(self.check_expr(x));
                        }
                        _ => {
                            let pos = self.pos;
                            self.error_expected(pos, "selector or type assertion");
                            self.next();
                            let sel = new_ident!(
                                self, pos, "_".to_string(), IdentEntity::NoEntity);
                            x = Expr::new_selector(x, sel);
                        }
                    }
                }
                Token::LBRACK => {
                    if lhs {
                        self.resolve(&x);
                    }
                    x = self.parse_index_or_slice(self.check_expr(x));
                }
                Token::LPAREN => {
                    if lhs {
                        self.resolve(&x);
                    }
                    x = self.parse_call_or_conversion(self.check_expr_or_type(x));
                }
                Token::LBRACE => {
                    if Parser::is_literal_type(&x) && 
                        (self.expr_level >= 0 || !Parser::is_type_name(&x)) {
                        if lhs {
                            self.resolve(&x);
                        }
                        x = self.parse_literal_value(Some(x));
                    } else {
                        break;
                    }
                }
                _ => {break;}
            }
            lhs = false; // no need to try to resolve again
        }
        
        self.trace_end();
        x
    }

    fn parse_unary_expr(&mut self, lhs: bool) -> Expr {
        self.trace_begin("UnaryExpr");

        let ret = match self.token {
            Token::ADD | Token::SUB | Token::NOT | Token::XOR | Token::AND => {
                let pos = self.pos;
                let op = self.token.clone();
                self.next();
                let x = self.parse_unary_expr(false);
                Expr::new_unary_expr(pos, op, self.check_expr(x))
            },
            Token::ARROW => {
                // channel type or receive expression
                let mut arrow = self.pos;
                self.next();

                // If the next token is token.CHAN we still don't know if it
                // is a channel type or a receive operation - we only know
                // once we have found the end of the unary expression. There
                // are two cases:
                //
                //   <- type  => (<-type) must be channel type
                //   <- expr  => <-(expr) is a receive from an expression
                //
                //   oxfeeefeee: a: [<- chan val_type_of_<-_chan]
                //               b: [<- chan val_type_of_chan]
                //
                // In the first case, the arrow must be re-associated with
                // the channel type parsed already:
                //
                //   <- (chan type)    =>  (<-chan type)
                //   <- (chan<- type)  =>  (<-chan (<-type))

                let mut x = self.parse_unary_expr(false);
                // determine which case we have
                if let Expr::Chan(c) = &mut x { // (<-type)
                    // re-associate position info and <-
                    let mut ctype = c.as_mut();
                    let mut dir = ChanDir::Send;
                    while dir == ChanDir::Send {
                        if ctype.dir == ChanDir::Recv {
                            // error: (<-type) is (<-(<-chan T))
                            self.error_expected(ctype.arrow, "'chan'")
                        }
                        let new_arrow = ctype.arrow;
                        ctype.begin = arrow;
                        ctype.arrow = arrow;
                        arrow = new_arrow;
                        dir = ctype.dir.clone();
                        ctype.dir = ChanDir::Recv;
                        if let Expr::Chan(c) = &mut ctype.val {
                            ctype = c.as_mut();
                        } else {
                            break;
                        }
                    }
                    if dir == ChanDir::Send {
                        self.error_expected(arrow, "channel type");
                    }
                    x
                } else {
                    Expr::new_unary_expr(arrow, Token::ARROW, self.check_expr(x))
                }
            },
            Token::MUL => {
                // pointer type or unary "*" expression
                let pos = self.pos;
                self.next();
                let x = self.parse_unary_expr(false);
                Expr::Star(Box::new(StarExpr{
                    star: pos, expr: self.check_expr_or_type(x)}))
            }
            _ => {
                self.parse_primary_expr(lhs)
            }
        };

        self.trace_end();
        ret
    }

    fn token_prec(&self) -> (Token, usize) {
        let token = if self.in_rhs && self.token == Token::ASSIGN {
            Token::EQL
        } else {
            self.token.clone()
        };
        let pre = token.precedence();
        (token, pre)
    }

    fn parse_binary_expr(&mut self, lhs: bool, prec1: usize) -> Expr {
        self.trace_begin("BinaryExpr");

        let mut x = self.parse_unary_expr(lhs);
        loop {
            let (op, prec) = self.token_prec();
            if prec < prec1 {
                break;
            }
            let pos = self.expect(&op);
            if lhs {
                self.resolve(&x);
            }
            let y = self.parse_binary_expr(false, prec+1);
            x = Expr::Binary(Box::new(BinaryExpr{
                expr_a: x, op_pos: pos, op: op, expr_b: y}))
        }

        self.trace_end();
        x
    }

    fn parse_expr(&mut self, lhs: bool) -> Expr {
        self.trace_begin("Expression");
        let x = self.parse_binary_expr(lhs, LOWEST_PREC+1);
        self.trace_end();
        x
    }

    fn parse_rhs(&mut self) -> Expr {
        let bak = self.in_rhs;
        self.in_rhs = true;
        let x0 = self.parse_expr(false);
        let x1 = self.check_expr(x0);
        self.in_rhs = bak;
        x1
    }

    fn parse_rhs_or_type(&mut self) -> Expr {
        let bak = self.in_rhs;
        self.in_rhs = true;
        let mut x = self.parse_expr(false);
        x = self.check_expr_or_type(x);
        self.in_rhs = bak;
        x
    }

    // ----------------------------------------------------------------------------
    // Statements
    
    // Parsing modes for parseSimpleStmt.
    const PSSTMT_BASIC: usize = 1;
    const PSSTMT_LABEL_OK: usize = 2;
    const PSSTMT_RANGE_OK: usize = 3;

    // parseSimpleStmt returns true as 2nd result if it parsed the assignment
    // of a range clause (with mode == rangeOk). The returned statement is an
    // assignment with a right-hand side that is a single unary expression of
    // the form "range x". No guarantees are given for the left-hand side.
    fn parse_simple_stmt(&mut self, mode: usize) -> (Stmt, bool) {
        self.trace_begin("SimpleStmt");
        let ret: Stmt;
        let mut is_range = false;

        let x = self.parse_lhs_list();
        match self.token {
            Token::DEFINE | Token::ASSIGN | Token::ADD_ASSIGN | Token::SUB_ASSIGN |
            Token::MUL_ASSIGN | Token::QUO_ASSIGN | Token::REM_ASSIGN |
            Token::AND_ASSIGN | Token::OR_ASSIGN | Token::XOR_ASSIGN | 
            Token::SHL_ASSIGN | Token::SHR_ASSIGN | Token::AND_NOT_ASSIGN => {
                // assignment statement, possibly part of a range clause
                let (mut pos, token) = (self.pos, self.token.clone());
                self.next();
                let y: Vec<Expr>;
                if mode == Parser::PSSTMT_RANGE_OK && self.token == Token::RANGE &&
                    (token == Token::DEFINE || token == Token::ASSIGN) {
                    pos = self.pos;
                    self.next();
                    y = vec![Expr::new_unary_expr(pos, Token::RANGE, self.parse_rhs())];
                    is_range = true;
                } else {
                    y = self.parse_rhs_list();
                }
                ret = Stmt::new_assign(&mut self.objects, x, pos, token.clone(), y);
                if token == Token::DEFINE {
                    self.short_var_decl(&ret);
                }
            }
            _ => {
                if x.len() > 1 {
                    self.error_expected(x[0].pos(&self.objects), "1 expression");
                    // continue with first expression
                }
                let x0 = x.into_iter().nth(0).unwrap();
                ret = match self.token {
                    Token::COLON => {
                        // labeled statement
                        let colon = self.pos;
                        self.next();
                        if mode == Parser::PSSTMT_LABEL_OK {
                            if let Expr::Ident(ident) = x0 {
                                // Go spec: The scope of a label is the body of the function
			                    // in which it is declared and excludes the body of any nested
			                    // function.
                                let s = self.parse_stmt();
                                let ls = LabeledStmt::arena_new(
                                    &mut self.objects, *ident.as_ref(), colon, s);
                                self.declare(
                                    DeclObj::LabeledStmt(ls), EntityData::NoData,
                                    EntityKind::Lbl, &self.label_scope.unwrap());
                                Stmt::Labeled(Box::new(ls.clone()))
                            } else {
                                self.error(colon, "illegal label declaration");
                                Stmt::new_bad(x0.pos(&self.objects), colon + 1)
                            }
                        } else {
                            self.error(colon, "illegal label declaration");
                            Stmt::new_bad(x0.pos(&self.objects), colon + 1)
                        }
                    },
                    Token::ARROW => {
                        let arrow = self.pos;
                        self.next();
                        let y = self.parse_rhs();
                        Stmt::Send(Box::new(SendStmt{chan: x0, arrow: arrow, val: y}))
                    },
                    Token::INC | Token::DEC => {
                        let s = Stmt::IncDec(Box::new(IncDecStmt{
                            expr: x0, token_pos: self.pos, token: self.token.clone()}));
                        self.next();
                        s
                    },
                    _ => {
                        Stmt::Expr(Box::new(x0))
                    }
                }
            }
        } 

        self.trace_end();
        (ret, is_range)
    }

    
    // todo
    fn parse_stmt(&mut self) -> Stmt {
        Stmt::new_bad(0, 0)
    }
    
    fn parse(&mut self) {
        self.trace_begin("begin");
        print!("222xxxxxxx \n");
        self.trace_end();
    }
}

#[cfg(test)]
mod test {
	use super::*;

	#[test]
	fn test_parser () {
        let fs = position::SharedFileSet::new();
        let mut fsm = fs.borrow_mut();
        let f = fsm.add_file(fs.weak(), "testfile1.gs", 0, 100);

        let mut p = Parser::new(f, "1 + 3 /  (3 + 4 + 5) * 6 + a.b", true);
        p.next();
        p.parse_rhs();
    }
} 