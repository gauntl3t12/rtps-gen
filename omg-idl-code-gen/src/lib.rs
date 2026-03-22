// Copyright (C) 2025  Bryan Conn
// Copyright (C) 2019  Frank Rehberger
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0>
mod ast;

use ast::*;
use omg_idl_grammar::{IdlParser, Rule};
use pest::{
    error::ErrorVariant,
    iterators::{Pair, Pairs},
    Parser, RuleType,
};
use std::{
    collections::HashSet,
    fs::File,
    io::{self, Read, Write},
    path::{Path, PathBuf},
};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum IdlError<R: RuleType> {
    #[error("Failed to parse IDL files")]
    ParserError(#[from] pest::error::Error<R>),
    #[error("Could not find requested idl_file: {0:#?}")]
    FileNotFound(PathBuf),
    #[error("Failed to render generated code.")]
    RenderError(#[from] minijinja::Error),
    #[error("Failed to write generated code.")]
    WriteError(#[from] io::Error),
}

/// All IDL Loader must be capable of reading data into the system
pub trait IdlLoader {
    fn load(&self, filename: &Path) -> Result<String, io::Error>;
}

/// Container for where to find a file and if extra logging occur
#[derive(Debug, Default)]
pub struct Configuration {
    search_path: PathBuf,
    idl_file: PathBuf,
    verbose: bool,
}

impl Configuration {
    pub fn new(search_path: &Path, idl_file: &Path, verbose: bool) -> Self {
        Self {
            search_path: search_path.to_path_buf(),
            idl_file: idl_file.to_path_buf(),
            verbose,
        }
    }
}

/// Vec to modules. Lower indexes are 'owners' of higher indexes.
type Scope = Vec<String>;

/// Container for the configuration & root module
#[derive(Debug, Clone)]
struct Context<'i> {
    config: &'i Configuration,
    root_module: IdlModule,
    included_files: HashSet<PathBuf>,
}

impl<'i> Context<'i> {
    pub fn new(config: &'i Configuration) -> Context<'i> {
        Context {
            config,
            root_module: IdlModule::new(None),
            included_files: HashSet::new(),
        }
    }

    fn normalize_path(path: PathBuf) -> PathBuf {
        path.canonicalize().unwrap_or(path)
    }

    /// Find the desired module under the root_module or create it if it
    /// does not already exist. The module request is made via the scope.
    fn lookup_module(&mut self, scope: &Scope) -> &mut IdlModule {
        // Starting from Root traverse the scope-path
        let mut current_module = &mut self.root_module;

        for name in scope {
            let submodule = current_module
                .modules
                .entry(name.to_owned())
                .or_insert(IdlModule::new(Some(name.to_owned())));
            current_module = submodule;
        }

        current_module
    }

    /// Add a new entry to the module for the discovered type
    fn add_type_dcl(&mut self, scope: &Scope, key: String, type_dcl: IdlTypeDcl) {
        self.lookup_module(scope)
            .types
            .entry(key)
            .or_insert(type_dcl);
    }

    /// Add a new entry to the module for the discovered const
    fn add_const_dcl(&mut self, scope: &Scope, key: String, const_dcl: IdlConstDcl) {
        self.lookup_module(scope)
            .constants
            .entry(key)
            .or_insert(const_dcl);
    }

    /// type_spec = { template_type_spec | simple_type_spec }
    pub fn read_type_spec(
        &mut self,
        scope: &Scope,
        pair: Pair<Rule>,
    ) -> Result<IdlTypeSpec, pest::error::Error<Rule>> {
        let rule = pair.as_rule();
        let pos = pair.as_span().start_pos();

        if self.config.verbose {
            print!("{:indent$}{:?}", "", rule, indent = 3 * scope.len());
        }

        let type_spec = match rule {
            Rule::float => IdlTypeSpec::F32Type,
            Rule::double => IdlTypeSpec::F64Type,
            Rule::long_double => IdlTypeSpec::F128Type,
            Rule::unsigned_short_int => IdlTypeSpec::U16Type,
            Rule::unsigned_longlong_int => IdlTypeSpec::U64Type,
            Rule::unsigned_long_int => IdlTypeSpec::U32Type,
            Rule::signed_short_int => IdlTypeSpec::I16Type,
            Rule::signed_longlong_int => IdlTypeSpec::I64Type,
            Rule::signed_long_int => IdlTypeSpec::I32Type,
            Rule::char_type => IdlTypeSpec::CharType,
            Rule::wide_char_type => IdlTypeSpec::WideCharType,
            Rule::boolean_type => IdlTypeSpec::BooleanType,
            Rule::octet_type => IdlTypeSpec::OctetType,
            Rule::string_type => match pair.into_inner().next() {
                None => IdlTypeSpec::StringType(None),
                Some(next_pair) => {
                    let pos_int_const = self.read_const_expr(scope, next_pair)?;
                    IdlTypeSpec::StringType(Some(Box::new(pos_int_const)))
                }
            },
            Rule::wide_string_type => {
                match pair.into_inner().next() {
                    None => IdlTypeSpec::WideStringType(None),
                    Some(next_pair) => {
                        let pos_int_const = self.read_const_expr(scope, next_pair)?;
                        IdlTypeSpec::WideStringType(Some(Box::new(pos_int_const)))
                        // Needs to be a &str
                    }
                }
            }
            Rule::sequence_type => {
                let pos = pair.as_span().start_pos();
                let mut inner = pair.into_inner();
                match (inner.next(), inner.next()) {
                    (Some(typ), None) => {
                        let typ_expr = self.read_type_spec(scope, typ)?;
                        Ok(IdlTypeSpec::SequenceType(Box::new(typ_expr)))
                    }
                    (Some(typ), Some(bound)) => {
                        let typ_expr = self.read_type_spec(scope, typ)?;
                        let _bound_expr = self.read_const_expr(scope, bound)?;
                        Ok(IdlTypeSpec::SequenceType(Box::new(typ_expr)))
                    }
                    _ => Err(pest::error::Error::new_from_pos(
                        ErrorVariant::CustomError {
                            message:
                                "Failed to discover required components to establish a sequence"
                                    .to_string(),
                        },
                        pos,
                    )),
                }?
            }
            //  scoped_name = { "::"? ~ identifier ~ ("::" ~ identifier)* }
            Rule::scoped_name => {
                let name = self.read_scoped_name(scope, pair)?;
                IdlTypeSpec::ScopedName(name)
            }
            // go deeper
            _ => match pair.into_inner().next() {
                Some(pair) => self.read_type_spec(scope, pair),
                _ => Err(pest::error::Error::new_from_pos(
                    ErrorVariant::CustomError {
                        message: "Failed find a deeper rule pair to follow".to_string(),
                    },
                    pos,
                )),
            }?,
        };

        Ok(type_spec)
    }

    /// declarator = { array_declarator | simple_declarator }
    /// array_declarator = { identifier ~ fixed_array_size+ }
    /// simple_declarator = { identifier }
    pub fn read_struct_member_declarator(
        &mut self,
        scope: &Scope,
        pair: Pair<Rule>,
        type_spec: &IdlTypeSpec,
    ) -> Result<IdlStructMember, pest::error::Error<Rule>> {
        let pos = pair.as_span().start_pos();
        match pair.into_inner().next() {
            Some(decl) => {
                let rule = decl.as_rule();
                let pos = decl.as_span().start_pos();
                if self.config.verbose {
                    print!(
                        "{:indent$}should be declarator {:?}",
                        "",
                        rule,
                        indent = 3 * scope.len()
                    );
                }
                let mut inner = decl.into_inner();
                match rule {
                    // simple_declarator = { identifier }
                    Rule::simple_declarator => match inner.next() {
                        Some(pair) => Ok(IdlStructMember {
                            id: self.read_identifier(scope, pair)?,
                            type_spec: type_spec.clone(),
                        }),
                        _ => Err(pest::error::Error::new_from_pos(
                            ErrorVariant::CustomError {
                                message: "Pair did not contain a valid IDL simple declarator"
                                    .to_string(),
                            },
                            pos,
                        )),
                    },
                    // array_declarator = { identifier ~ fixed_array_size+ }
                    Rule::array_declarator => {
                        match inner.next() {
                            Some(pair) => {
                                let array_sizes: Result<Vec<_>, pest::error::Error<Rule>> = inner
                                    .map(|pair| {
                                            match pair.into_inner().next() {
                                                Some(pair) => {
                                                    // skip node Rule::fixed_array_size and read const_expr underneath
                                                    self.read_const_expr(scope, pair)
                                                },
                                                _ => {
                                                    Err(pest::error::Error::new_from_pos(
                                                            ErrorVariant::CustomError {
                                                                message: "Pair did not contain a valid 'const expression'".to_string(),
                                                            }, pos))
                                                }
                                            }
                                    })
                                    .collect();
                                let array_type_spec = IdlTypeSpec::ArrayType(
                                    Box::new(type_spec.clone()),
                                    array_sizes?,
                                );

                                Ok(IdlStructMember {
                                    id: self.read_identifier(scope, pair)?,
                                    type_spec: array_type_spec,
                                })
                            }
                            _ => Err(pest::error::Error::new_from_pos(
                                ErrorVariant::CustomError {
                                    message: "Pair did not contain a valid IDL array declatator"
                                        .to_string(),
                                },
                                pos,
                            )),
                        }
                    }
                    _ => Err(pest::error::Error::new_from_pos(
                        ErrorVariant::CustomError {
                            message: "Pair did not contain a valid IDL type rule".to_string(),
                        },
                        pos,
                    )),
                }
            }
            _ => Err(pest::error::Error::new_from_pos(
                ErrorVariant::CustomError {
                    message: "Pair did not contain a either other a simple or arraty declarator"
                        .to_string(),
                },
                pos,
            )),
        }
    }

    // member = { type_spec ~ declarators ~ ";" }
    // declarators = { declarator ~ ("," ~ declarator )* }
    // declarator = { array_declarator | simple_declarator }
    fn read_struct_member(
        &mut self,
        scope: &Scope,
        pair: Pair<Rule>,
    ) -> Result<Vec<IdlStructMember>, pest::error::Error<Rule>> {
        let pos = pair.as_span().start_pos();

        if self.config.verbose {
            print!(
                "{:indent$}{:?}",
                "",
                pair.as_rule(),
                indent = 3 * scope.len()
            );
        }

        let mut inner = pair.into_inner();
        let type_spec = match inner.next() {
            Some(pair) => self.read_type_spec(scope, pair),
            _ => Err(pest::error::Error::new_from_pos(
                ErrorVariant::CustomError {
                    message: "Pair did not contain a valid IDL type rule".to_string(),
                },
                pos,
            )),
        }?;

        // skip rule 'declarators' and parse sibblings `declarator'
        let declarators = match inner.next() {
            Some(pair) => Ok(pair.into_inner()),
            _ => Err(pest::error::Error::new_from_pos(
                ErrorVariant::CustomError {
                    message: "Failed to aquire next declaration pair".to_string(),
                },
                pos,
            )),
        }?;

        declarators
            .map(|declarator| self.read_struct_member_declarator(scope, declarator, &type_spec))
            .collect()
    }

    /// identifier = @{ (alpha | "_") ~ ("_" | alpha | digit)* }
    fn read_identifier(
        &mut self,
        scope: &Scope,
        pair: Pair<Rule>,
    ) -> Result<String, pest::error::Error<Rule>> {
        let rule = pair.as_rule();
        if self.config.verbose {
            println!("{:indent$}{:?}", "", rule, indent = 3 * scope.len());
        }
        match rule {
            Rule::identifier | Rule::enumerator => Ok(pair.as_str().to_owned()),
            _ => Err(pest::error::Error::new_from_pos(
                ErrorVariant::CustomError {
                    message: "Pair did not contain a valid scoped name rule".to_string(),
                },
                pair.as_span().start_pos(),
            )),
        }
    }

    /// scoped_name = { "::"? ~ identifier ~ ("::" ~ identifier)* }
    fn read_scoped_name(
        &mut self,
        scope: &Scope,
        pair: Pair<Rule>,
    ) -> Result<IdlScopedName, pest::error::Error<Rule>> {
        let is_absolute_name = pair.as_str().starts_with("::");
        if self.config.verbose {
            println!(
                "{:indent$}>>> {:?} '{}' - abs? {is_absolute_name}",
                "",
                pair.as_rule(),
                pair.as_str(),
                indent = 3 * scope.len()
            );
        }

        // check if name starts with "::"
        let inner = pair.into_inner();
        let scoped_name: Result<Vec<String>, pest::error::Error<Rule>> = inner
            .map(|pair| self.read_identifier(scope, pair))
            .collect();

        Ok(IdlScopedName(scoped_name?, is_absolute_name))
    }

    /// const_expr = { unary_expr ~ (or_expr | xor_expr | and_expr | shift_expr | add_expr | mult_expr)? }
    fn read_const_expr(
        &mut self,
        scope: &Scope,
        pair: Pair<Rule>,
    ) -> Result<IdlValueExpr, pest::error::Error<Rule>> {
        let rule = pair.as_rule();
        let pos = pair.as_span().start_pos();

        if self.config.verbose {
            println!(
                "{:indent$}{:?} '{}'",
                "",
                rule,
                pair.as_str(),
                indent = 3 * scope.len()
            );
        }
        let fp_collect_init = (None, None, None, None);

        let fp_collect = |(i, f, e, s), node: Pair<Rule>| match node.as_rule() {
            Rule::integral_part => (Some(node.as_str().to_owned()), f, e, s),
            Rule::fractional_part => (i, Some(node.as_str().to_owned()), e, s),
            Rule::exponent => (i, f, Some(node.as_str().to_owned()), s),
            Rule::float_suffix => (i, f, e, Some(node.as_str().to_owned())),
            _ => panic!(),
        };

        let mut binary_op_collect =
            |pair: Option<Pair<'_, Rule>>, bin_op: BinaryOp, err_str: &str| match pair {
                Some(pair) => {
                    let expr = self.read_const_expr(scope, pair)?;
                    Ok(IdlValueExpr::BinaryOp(bin_op, Box::new(expr)))
                }
                None => Err(pest::error::Error::new_from_pos(
                    ErrorVariant::CustomError {
                        message: format!("No associated values found with the parsed '{err_str}'"),
                    },
                    pos,
                )),
            };

        let pair_as_str = pair.as_str();
        let mut inner = pair.into_inner();
        match rule {
            Rule::const_expr => match (inner.next(), inner.next()) {
                (Some(expr0), Some(expr1)) => {
                    let value_expr_0 = self.read_const_expr(scope, expr0)?;
                    let value_expr_1 = self.read_const_expr(scope, expr1)?;
                    Ok(IdlValueExpr::Expr(Box::new(value_expr_0), Box::new(value_expr_1)))
                }
                (Some(expr1), None) => self.read_const_expr(scope, expr1),
                _ => Err(pest::error::Error::new_from_pos(
                    ErrorVariant::CustomError {
                        message: "Pair did not contain a valid const expression rule".to_string(),
                    }, pos)),
            },
            Rule::unary_expr => match (inner.next(), inner.next()) {
                (Some(unary_op), Some(prim_expr)) => {
                    let pos = prim_expr.as_span().start_pos();
                    let expr = self.read_const_expr(scope, prim_expr)?;
                    match unary_op.as_str() {
                        "-" => Ok(IdlValueExpr::UnaryOp(UnaryOp::Neg, Box::new(expr))),
                        "+" => Ok(IdlValueExpr::UnaryOp(UnaryOp::Pos, Box::new(expr))),
                        "~" => Ok(IdlValueExpr::UnaryOp(UnaryOp::Inverse, Box::new(expr))),
                        _ => Err(pest::error::Error::new_from_pos(
                            ErrorVariant::CustomError {
                                message: format!("{unary_op} does not match acceptable values -|+|~"),
                            }, pos)),
                    }
                }
                (Some(prim_expr), None) => {
                    self.read_const_expr(scope, prim_expr)
                },
                _ => Err(pest::error::Error::new_from_pos(
                    ErrorVariant::CustomError {
                        message: "Rule does not match expected unary expression format".to_string(),
                    }, pos)),
            },
            Rule::primary_expr => match inner.next() {
                //  scoped_name = { "::"? ~ identifier ~ ("::" ~ identifier)* }
                Some(pair) if pair.as_rule() == Rule::scoped_name => {
                    let name = self.read_scoped_name(scope, pair)?;
                    Ok(IdlValueExpr::ScopedName(name))
                }
                Some(pair) if pair.as_rule() == Rule::literal => {
                    self.read_const_expr(scope, pair)
                },
                Some(pair) if pair.as_rule() == Rule::const_expr => {
                    let expr = self.read_const_expr(scope, pair)?;
                    Ok(IdlValueExpr::Brace(Box::new(expr)))
                }
                _ => Err(pest::error::Error::new_from_pos(
                    ErrorVariant::CustomError {
                        message: "Primary expression did not match 'scoped_name', 'literal', or 'const expression'".to_string(),
                    }, pos)),
            },
            Rule::and_expr => {
                binary_op_collect(inner.next(), BinaryOp::And, "and expression")
            }
            Rule::or_expr => {
                binary_op_collect(inner.next(), BinaryOp::Or, "or expression")
            }
            Rule::xor_expr => {
                binary_op_collect(inner.next(), BinaryOp::Xor, "xor expression")
            }
            Rule::lshift_expr => {
                binary_op_collect(inner.next(), BinaryOp::LShift, "left shift expression")
            }
            Rule::rshift_expr => {
                binary_op_collect(inner.next(), BinaryOp::RShift, "right shift expression")
            }
            Rule::add_expr => {
                binary_op_collect(inner.next(), BinaryOp::Add, "add expression")
            }
            Rule::sub_expr => {
                binary_op_collect(inner.next(), BinaryOp::Sub, "sub expression")
            }
            Rule::mul_expr => {
                binary_op_collect(inner.next(), BinaryOp::Mul, "multiply expression")
            }
            Rule::div_expr => {
                binary_op_collect(inner.next(), BinaryOp::Div, "division expression")
            }
            Rule::mod_expr => {
                binary_op_collect(inner.next(), BinaryOp::Mod, "modulo expression")
            }
            Rule::decimal_integer_literal => Ok(IdlValueExpr::DecLiteral(pair_as_str.to_owned())),
            Rule::octal_integer_literal => Ok(IdlValueExpr::OctLiteral(pair_as_str.to_owned())),
            Rule::hex_integer_literal => Ok(IdlValueExpr::HexLiteral(pair_as_str.to_owned())),
            Rule::floating_pt_literal => {
                let (i, f, e, s) = inner.fold(fp_collect_init, fp_collect);
                Ok(IdlValueExpr::FloatLiteral(i, f, e, s))
            }
            Rule::boolean_literal => {
                let true_str = "TRUE".to_string();
                Ok(IdlValueExpr::BooleanLiteral(pair_as_str.to_uppercase() == true_str))
            },
            Rule::character_literal => Ok(IdlValueExpr::CharLiteral(pair_as_str.to_owned())),
            Rule::wide_character_literal => {
                Ok(IdlValueExpr::WideCharLiteral(pair_as_str.to_owned()))
            }
            Rule::string_literal => Ok(IdlValueExpr::StringLiteral(pair_as_str.to_owned())),
            Rule::wide_string_literal => {
                Ok(IdlValueExpr::WideStringLiteral(pair_as_str.to_owned()))
            }
            _ => {
                match inner.next() {
                    Some(pair) => {
                        self.read_const_expr(scope, pair)
                    }
                    None => {
                        Err(pest::error::Error::new_from_pos(
                        ErrorVariant::CustomError {
                            message: "Failed to read 'const expression' in the catch all".to_string(),
                        }, pos))
                    }
                }
            },
        }
    }

    /// declarator = { array_declarator | simple_declarator }
    /// array_declarator = { identifier ~ fixed_array_size+ }
    /// simple_declarator = { identifier }
    fn read_switch_element_declarator(
        &mut self,
        scope: &Scope,
        pair: Pair<Rule>,
        type_spec: &IdlTypeSpec,
    ) -> Result<IdlSwitchElement, pest::error::Error<Rule>> {
        let pos = pair.as_span().start_pos();

        match pair.into_inner().next() {
            Some(decl) => {
                let rule = decl.as_rule();
                let pos = decl.as_span().start_pos();
                if self.config.verbose {
                    println!(
                        "{:indent$}should be declarator {:?}",
                        "",
                        rule,
                        indent = 3 * scope.len()
                    );
                }

                let mut inner = decl.into_inner();
                match rule {
                    // simple_declarator = { identifier }
                    Rule::simple_declarator => {
                        match inner.next() {
                            Some(pair) => {
                                Ok(IdlSwitchElement {
                                    id: self.read_identifier(scope, pair)?,
                                    type_spec: type_spec.clone(),
                                })
                            }
                            _ => {
                                Err(pest::error::Error::<Rule>::new_from_pos(
                                    ErrorVariant::CustomError {
                                        message: "Failed to discover simple declarator for switch element".to_string(),
                                    }, pos))
                            },
                        }
                    }
                    // array_declarator = { identifier ~ fixed_array_size+ }
                    Rule::array_declarator => {
                        match inner.next() {
                            Some(pair) => {
                                let id = self.read_identifier(scope, pair)?;
                                let array_sizes: Result<Vec<_>, pest::error::Error<Rule>> = inner
                                    .map(|pair| {
                                            let pos = pair.as_span().start_pos();
                                            match pair.into_inner().next() {
                                                Some(pair) => {
                                                    // skip node Rule::fixed_array_size and read const_expr underneath
                                                    self.read_const_expr(scope, pair)
                                                }
                                                _ => Err(pest::error::Error::new_from_pos(
                                                        ErrorVariant::CustomError {
                                                            message: "Failed to discover const_expr under the fixed_array_size for switch element declarator".to_string(),
                                                        }, pos)),
                                            }
                                    })
                                    .collect();
                                let array_type_spec =
                                    IdlTypeSpec::ArrayType(Box::new(type_spec.clone()), array_sizes?);

                                Ok(IdlSwitchElement {
                                    id,
                                    type_spec: array_type_spec,
                                })
                            }
                            _ => Err(pest::error::Error::new_from_pos(
                                    ErrorVariant::CustomError {
                                        message: "Failed to parse array declarator for switch element declarator".to_string(),
                                    }, pos)),
                        }
                    },
                    _ => Err(pest::error::Error::new_from_pos(
                            ErrorVariant::CustomError {
                                message: "Failed to discover either simple or array declarator for switch element declarator".to_string(),
                            }, pos)),

                }
            }
            _ => Err(pest::error::Error::new_from_pos(
                ErrorVariant::CustomError {
                    message: "Failed to parse switch element declarator".to_string(),
                },
                pos,
            )),
        }
    }

    /// element_spec = { type_spec ~ declarator }
    fn read_switch_element_spec(
        &mut self,
        scope: &Scope,
        pair: Pair<Rule>,
    ) -> Result<IdlSwitchElement, pest::error::Error<Rule>> {
        let rule = pair.as_rule();
        let pos = pair.as_span().start_pos();
        if self.config.verbose {
            println!("{:indent$}{:?}", "", rule, indent = 3 * scope.len());
        }
        let mut inner = pair.into_inner();
        match inner.next() {
            Some(pair) => {
                let type_spec = self.read_type_spec(scope, pair)?;
                match inner.next() {
                    Some(pair) => self.read_switch_element_declarator(scope, pair, &type_spec),
                    _ => Err(pest::error::Error::new_from_pos(
                        ErrorVariant::CustomError {
                            message: "Failed to read declarator from the switch element spec"
                                .to_string(),
                        },
                        pos,
                    )),
                }
            }
            _ => Err(pest::error::Error::new_from_pos(
                ErrorVariant::CustomError {
                    message: "Failed to read type spec from the switch element spec".to_string(),
                },
                pos,
            )),
        }
    }

    /// switch_type_spec = {integer_type | char_type | boolean_type | wide_char_type | octet_type | scoped_name }
    fn read_switch_type_spec(
        &mut self,
        scope: &Scope,
        pair: Pair<Rule>,
    ) -> Result<IdlTypeSpec, pest::error::Error<Rule>> {
        let rule = pair.as_rule();
        let pos = pair.as_span().start_pos();
        if self.config.verbose {
            println!("{:indent$}{:?}", "", rule, indent = 3 * scope.len());
        }
        match pair.into_inner().next() {
            Some(pair) => self.read_type_spec(scope, pair),
            _ => Err(pest::error::Error::new_from_pos(
                ErrorVariant::CustomError {
                    message: "Failed to read associated type for switch type".to_string(),
                },
                pos,
            )),
        }
    }

    /// switch_body = { case+ }
    fn read_switch_body(
        &mut self,
        scope: &Scope,
        pair: Pair<Rule>,
    ) -> Result<Vec<IdlSwitchCase>, pest::error::Error<Rule>> {
        let rule = pair.as_rule();
        if self.config.verbose {
            println!("{:indent$}{:?}", "", rule, indent = 3 * scope.len());
        }

        pair.into_inner()
            .map(|pair| self.read_switch_case(scope, pair))
            .collect()
    }

    /// case_label = { "case" ~ const_expr ~ ":" | "default" ~ ":" }
    fn read_switch_label(
        &mut self,
        scope: &Scope,
        pair: Pair<Rule>,
    ) -> Result<IdlSwitchLabel, pest::error::Error<Rule>> {
        if self.config.verbose {
            println!(
                "{:indent$}{:?}",
                "",
                pair.as_rule(),
                indent = 3 * scope.len()
            );
        }

        match pair.into_inner().next() {
            Some(pair) => {
                let expr = self.read_const_expr(scope, pair)?;
                Ok(IdlSwitchLabel::Label(expr))
            }
            _ => Ok(IdlSwitchLabel::Default),
        }
    }

    /// case = { case_label+ ~ element_spec ~ ";" }
    fn read_switch_case(
        &mut self,
        scope: &Scope,
        pair: Pair<Rule>,
    ) -> Result<IdlSwitchCase, pest::error::Error<Rule>> {
        if self.config.verbose {
            println!(
                "{:indent$}{:?}",
                "",
                pair.as_rule(),
                indent = 3 * scope.len()
            );
        }

        let inner = pair.into_inner();
        let case_labels: Result<Vec<IdlSwitchLabel>, pest::error::Error<Rule>> = inner
            .clone()
            .filter(|p| p.as_rule() == Rule::case_label)
            .map(|p| self.read_switch_label(scope, p))
            .collect();

        // there will be only one in the list, choose the last
        let elem_spec = inner
            .filter(|p| p.as_rule() == Rule::element_spec)
            .map(|p| self.read_switch_element_spec(scope, p))
            .last()
            .unwrap();

        Ok(IdlSwitchCase {
            labels: case_labels?,
            elem_spec: elem_spec?,
        })
    }

    /// declarator = { array_declarator | simple_declarator }
    /// array_declarator = { identifier ~ fixed_array_size+ }
    /// simple_declarator = { identifier }
    fn process_declarator(
        &mut self,
        scope: &Scope,
        pair: Pair<Rule>,
        type_spec: &IdlTypeSpec,
    ) -> Result<(), pest::error::Error<Rule>> {
        let pos = pair.as_span().start_pos();
        match pair.into_inner().next() {
            Some(decl) => {
                let rule = decl.as_rule();
                let pos = decl.as_span().start_pos();
                if self.config.verbose {
                    println!("{:indent$}{:?}", "", rule, indent = 3 * scope.len());
                }
                let mut inner = decl.clone().into_inner();

                match rule {
                    Rule::simple_declarator => {
                        match inner.next() {
                            Some(pair) => {
                                let id = self.read_identifier(scope, pair)?;
                                let type_dcl = IdlTypeDcl(IdlTypeDclKind::TypeDcl(id.clone(), type_spec.clone()));
                                self.add_type_dcl(scope, id, type_dcl);
                                Ok(())
                            },
                            _ => {
                                Err(pest::error::Error::new_from_pos(
                                ErrorVariant::CustomError {
                                    message: "Pair did not contain a valid identifer for the simple declarator".to_string(),
                                }, pos))
                            }
                        }
                    },
                    // array_declarator = { identifier ~ fixed_array_size+ }
                    Rule::array_declarator => {
                        match inner.next() {
                            Some(pair) => {
                                let id = self.read_identifier(scope, pair)?;
                                let key = id.clone();

                                let array_sizes: Result<Vec<_>, pest::error::Error<Rule>> = inner
                                    .map(|pair| {
                                        match pair.into_inner().next() {
                                            Some(pair) => {
                                                // skip node Rule::fixed_array_size and read const_expr underneath
                                                self.read_const_expr(scope, pair)
                                            },
                                            _ => {
                                                Err(pest::error::Error::new_from_pos(
                                                ErrorVariant::CustomError {
                                                    message: "Failed to read the const expr under the Rule::fixed_array_size".to_string(),
                                                }, pos))
                                            }
                                        }
                                    })
                                    .collect();
                                let array_type_spec =
                                    IdlTypeSpec::ArrayType(Box::new(type_spec.clone()), array_sizes?);
                                let type_dcl = IdlTypeDcl(IdlTypeDclKind::TypeDcl(id, array_type_spec));
                                self.add_type_dcl(scope, key, type_dcl);
                                Ok(())
                            },
                            _ => {
                                Err(pest::error::Error::new_from_pos(
                                ErrorVariant::CustomError {
                                    message: "Pair did not contain a valid identifer for the simple declarator".to_string(),
                                }, pos))
                            }
                        }
                    },
                    _ => {
                        Err(pest::error::Error::new_from_pos(
                        ErrorVariant::CustomError {
                            message: "Pair did not contain a valid simple or array declarator".to_string(),
                        }, pos))
                    }
                }
            }
            // traverse deeper
            _ => Err(pest::error::Error::new_from_pos(
                ErrorVariant::CustomError {
                    message: "No associated values found with the parsed array|simple declarator"
                        .to_string(),
                },
                pos,
            )),
        }
    }

    /// Walk through all discovered pairs and create the associated objs
    fn process<L: IdlLoader>(
        &mut self,
        scope: &mut Scope,
        loader: &mut dyn IdlLoader,
        pair: Pair<Rule>,
    ) -> Result<(), IdlError<Rule>> {
        let mut iter = pair.clone().into_inner();
        if self.config.verbose {
            println!(
                "{:indent$}{:?}",
                "",
                pair.as_rule(),
                indent = 3 * scope.len()
            );
        }
        match pair.as_rule() {
            // module_dcl = { "module" ~ identifier ~ "{" ~ definition* ~ "}" }
            Rule::module_dcl => {
                let id = iter.next().unwrap().as_str();

                scope.push(id.to_owned());

                let _ = self.lookup_module(scope);

                for p in iter {
                    let _ = self.process::<L>(scope, loader, p);
                }

                let _ = scope.pop();

                Ok(())
            }
            // struct_def = { "struct" ~ identifier ~ (":" ~ scoped_name)? ~ "{" ~ member* ~ "}" }
            Rule::struct_def => {
                let id = iter.next().unwrap().as_str().to_owned();
                let key = id.clone();
                let m1: Result<Vec<Vec<IdlStructMember>>, _> = iter
                    .map(|p| {
                        // skip the member-node and read sibbling directly
                        self.read_struct_member(scope, p)
                    })
                    .collect();

                let m2 = m1?;
                let members = m2.into_iter().flatten().collect::<Vec<_>>();

                let typedcl = IdlTypeDcl(IdlTypeDclKind::StructDcl(id, members));
                self.add_type_dcl(scope, key, typedcl);
                Ok(())
            }
            // union_def = { "union" ~ identifier ~ "switch" ~ "(" ~ switch_type_spec ~ ")" ~ "{" ~ switch_body ~ "}" }
            Rule::union_def => {
                let id = self.read_identifier(scope, iter.next().unwrap())?;
                let key = id.to_owned();
                let switch_type_spec = self.read_switch_type_spec(scope, iter.next().unwrap())?;
                let switch_body = self.read_switch_body(scope, iter.next().unwrap())?;
                let union_def =
                    IdlTypeDcl(IdlTypeDclKind::UnionDcl(id, switch_type_spec, switch_body));

                self.add_type_dcl(scope, key, union_def);
                Ok(())
            }
            // type_declarator = { (template_type_spec | constr_type_dcl | simple_type_spec) ~ any_declarators }
            Rule::type_declarator => {
                let type_spec = self.read_type_spec(scope, iter.next().unwrap())?;

                let any_declarators_pair = &iter.next().unwrap();

                for p in any_declarators_pair.clone().into_inner() {
                    let _ = self.process_declarator(scope, p, &type_spec);
                }
                Ok(())
            }
            // enum_dcl = { "enum" ~ identifier ~ "{" ~ enumerator ~ ("," ~ enumerator)* ~ ","? ~ "}" }
            // enumerator = { identifier }
            Rule::enum_dcl => {
                let id = iter.next().unwrap().as_str().to_owned();
                let key = id.clone();
                let enums: Result<Vec<_>, pest::error::Error<Rule>> =
                    iter.map(|p| self.read_identifier(scope, p)).collect();

                let typedcl = IdlTypeDcl(IdlTypeDclKind::EnumDcl(id, enums?));
                self.add_type_dcl(scope, key, typedcl);
                Ok(())
            }
            // const_dcl = { "const" ~ const_type ~ identifier ~ "=" ~ const_expr }
            Rule::const_dcl => {
                let type_spec = self.read_type_spec(scope, iter.next().unwrap())?;
                let id = self.read_identifier(scope, iter.next().unwrap())?;
                let key = id.clone();
                let const_expr = self.read_const_expr(scope, iter.next().unwrap())?;
                let const_dcl = IdlConstDcl {
                    id,
                    typedcl: type_spec,
                    value: const_expr,
                };
                self.add_const_dcl(scope, key, const_dcl);
                Ok(())
            }
            // include_directive = !{ "#" ~ "include" ~ (("<" ~ path_spec ~ ">") | ("\"" ~ path_spec ~ "\"")) }
            Rule::include_directive => {
                if let Some(ref p) = pair.clone().into_inner().nth(0) {
                    let fname = PathBuf::from(p.as_str());
                    let full_path = Self::normalize_path(self.config.search_path.join(&fname));
                    if !self.included_files.insert(full_path.clone()) {
                        return Ok(());
                    }

                    let data = loader
                        .load(&fname)
                        .map_err(|_| IdlError::FileNotFound(fname))?;

                    let idl: Pairs<Rule> = IdlParser::parse(Rule::specification, &data)?;
                    for p in idl {
                        self.process::<L>(scope, loader, p)?;
                    }
                }
                Ok(())
            }
            // anything else
            _ => {
                for p in iter {
                    let _ = self.process::<L>(scope, loader, p);
                }
                Ok(())
            }
        }
    }

    // enum_dcl = { "enum" ~ identifier ~ "{" ~ enumerator ~ ("," ~ enumerator)* ~ ","? ~ "}" }
    // enumerator = { identifier }
}

/// Provided w/ an object that supports writing, an IDL Loader, and an OMG Gen Config,
/// generate Rust Types for the requested OMG IDL files.
///
/// @param out: An object that supports writing
/// @param loader: Library Object to read IDL
/// @param config: Library config
fn generate_with_loader<W: Write, L: IdlLoader>(
    out: &mut W,
    loader: &mut L,
    config: &Configuration,
) -> Result<(), IdlError<Rule>> {
    let mut ctx = Context::new(config);
    let root_path = Context::normalize_path(config.search_path.join(&config.idl_file));
    ctx.included_files.insert(root_path);

    let idl_file = config.idl_file.clone();
    let idl_file_data = loader
        .load(&idl_file)
        .map_err(|_| IdlError::FileNotFound(idl_file))?;

    let mut scope = Scope::new();
    let idl: Pairs<Rule> = IdlParser::parse(Rule::specification, &idl_file_data)?;

    for p in idl {
        let _ = ctx.process::<L>(&mut scope, loader, p);
    }

    let mut env = minijinja::Environment::new();
    minijinja_embed::load_templates!(&mut env);
    let root_module_text = ctx.root_module.render(&env, 0)?;

    Ok(write!(out, "{root_module_text}")?)
}

/// Object used to input the request IDL file into the library.
#[derive(Debug, Clone, Default)]
struct Loader {
    search_path: PathBuf,
}

impl Loader {
    pub fn new(search_path: &Path) -> Self {
        Self {
            search_path: search_path.to_path_buf(),
        }
    }
}

impl IdlLoader for Loader {
    /// Read the requested file and return as a Result<String>
    fn load(&self, filename: &Path) -> Result<String, io::Error> {
        let fullname = self.search_path.join(filename);
        let mut file = File::open(fullname)?;
        let mut data = String::new();

        file.read_to_string(&mut data)?;

        Ok(data)
    }
}

/// Provided w/ an object that supports writing and a OMG Gen Config
/// generate Rust Types for the requested OMG IDL files.
///
/// @param out: An object that supports writing
/// @param config: Library config
pub fn generate_with_search_path<W: Write>(
    out: &mut W,
    config: &Configuration,
) -> Result<(), IdlError<Rule>> {
    let mut loader = Loader::new(&config.search_path);
    let mut env = minijinja::Environment::new();
    minijinja_embed::load_templates!(&mut env);
    generate_with_loader(out, &mut loader, config)
}
