use std::collections::{BTreeMap, HashSet};
use std::error::Error;

use super::clang;
use clang::*;

#[derive(Clone, Debug)]
pub struct Namespace {
    pub children: BTreeMap<String, Namespace>,
    pub typedefs: Vec<Typedef>,
    pub records: Vec<Record>,
    pub constants: Vec<Constant>,
    unnamed_record_counter: usize,
}

impl Namespace {
    pub fn new() -> Namespace {
        Namespace {
            children: BTreeMap::new(),
            typedefs: Vec::new(),
            records: Vec::new(),
            constants: Vec::new(),
            unnamed_record_counter: 0,
        }
    }

    pub fn parse(cursor: &Cursor, skip_list: &[&str]) -> Result<Namespace, Box<dyn Error>> {
        let mut parser = Parser::new(skip_list);
        let mut namespace = Namespace::new();

        cursor.visit_children(|cursor| parser.visit(&mut namespace, cursor))?;

        Ok(namespace)
    }

    pub fn is_empty(&self) -> bool {
        self.typedefs.is_empty()
            && self.records.is_empty()
            && self.children.values().all(|child| child.is_empty())
    }
}

#[derive(Clone, Debug)]
pub struct Typedef {
    pub name: String,
    pub type_: Type,
}

#[derive(Copy, Clone, Eq, PartialEq, Debug)]
pub enum RecordKind {
    Struct,
    Union,
}

#[derive(Clone, Debug)]
pub struct Record {
    pub name: String,
    pub kind: RecordKind,
    pub fields: Vec<Field>,
    pub bases: Vec<String>,
    pub virtual_methods: Vec<Method>,
    pub inner: Namespace,
}

#[derive(Clone, Debug)]
pub struct Field {
    pub name: String,
    pub type_: Type,
}

#[derive(Clone, Debug)]
pub struct Method {
    pub name: String,
    pub arguments: Vec<Argument>,
    pub result_type: Type,
}

#[derive(Clone, Debug)]
pub struct Argument {
    pub name: String,
    pub type_: Type,
}

#[derive(Clone, Debug)]
pub struct Constant {
    pub name: String,
    pub type_: Type,
    pub value: Value,
}

#[derive(Clone, Debug)]
pub enum Type {
    Void,
    Bool,
    Char,
    UChar,
    UShort,
    UInt,
    ULong,
    ULongLong,
    SChar,
    Short,
    Int,
    Long,
    LongLong,
    Unsigned(usize),
    #[allow(dead_code)]
    Signed(usize),
    Float,
    Double,
    Pointer {
        is_const: bool,
        pointee: Box<Type>,
    },
    Reference {
        is_const: bool,
        pointee: Box<Type>,
    },
    Record(String),
    Typedef(String),
    Array(usize, Box<Type>),
}

#[derive(Clone, Debug)]
pub enum Value {
    Signed(i64),
    Unsigned(u64),
}

struct Parser {
    skip_list: HashSet<String>,
}

impl Parser {
    fn new(skip_list: &[&str]) -> Parser {
        let skip_list = skip_list
            .iter()
            .map(|s| s.to_string())
            .collect::<HashSet<_>>();

        Parser { skip_list }
    }

    fn visit(&mut self, namespace: &mut Namespace, cursor: &Cursor) -> Result<(), Box<dyn Error>> {
        if cursor.is_in_system_header() {
            return Ok(());
        }

        if self.skip_list.contains(cursor.name().to_str().unwrap()) {
            return Ok(());
        }

        match cursor.kind() {
            CursorKind::Namespace => {
                let name = cursor.name();
                let name_str = name.to_str().unwrap();

                // Skip the contents of unnamed namespaces
                if name_str.is_empty() {
                    return Ok(());
                }

                if !namespace.children.contains_key(name_str) {
                    namespace
                        .children
                        .insert(name_str.to_string(), Namespace::new());
                }
                let child_namespace = namespace.children.get_mut(name_str).unwrap();
                cursor.visit_children(|cursor| self.visit(child_namespace, cursor))?;
            }
            CursorKind::TypedefDecl | CursorKind::TypeAliasDecl => {
                let typedef = cursor.type_().unwrap();
                let name = typedef.typedef_name();

                let type_ = self.parse_type(
                    cursor.typedef_underlying_type().unwrap(),
                    cursor.location(),
                    namespace,
                )?;

                namespace.typedefs.push(Typedef {
                    name: name.unwrap().to_str().unwrap().to_string(),
                    type_,
                });
            }
            CursorKind::EnumDecl => {
                let name = cursor.name();
                let name_str = name.to_str().unwrap();

                let int_type = self.parse_type(
                    cursor.enum_integer_type().unwrap(),
                    cursor.location(),
                    namespace,
                )?;

                if !name_str.is_empty() {
                    namespace.typedefs.push(Typedef {
                        name: name_str.to_string(),
                        type_: int_type.clone(),
                    });
                }

                let canonical_type = cursor.enum_integer_type().unwrap().canonical_type();
                let signed = match canonical_type.kind() {
                    TypeKind::Char_U
                    | TypeKind::UChar
                    | TypeKind::UShort
                    | TypeKind::UInt
                    | TypeKind::ULong
                    | TypeKind::ULongLong => false,
                    TypeKind::Char_S
                    | TypeKind::SChar
                    | TypeKind::Short
                    | TypeKind::Int
                    | TypeKind::Long
                    | TypeKind::LongLong => true,
                    _ => return Err(format!("unhandled enum type {:?}", int_type).into()),
                };

                cursor.visit_children(|cursor| -> Result<(), Box<dyn Error>> {
                    match cursor.kind() {
                        CursorKind::EnumConstantDecl => {
                            let value = if signed {
                                Value::Signed(cursor.enum_constant_value().unwrap())
                            } else {
                                Value::Unsigned(cursor.enum_constant_value_unsigned().unwrap())
                            };

                            namespace.constants.push(Constant {
                                name: cursor.name().to_str().unwrap().to_string(),
                                type_: int_type.clone(),
                                value,
                            });
                        }
                        _ => {}
                    }

                    Ok(())
                })?;
            }
            CursorKind::StructDecl | CursorKind::UnionDecl | CursorKind::ClassDecl => {
                if cursor.is_definition() {
                    // Skip unnamed records here, as parse_type will take care of them
                    if !cursor.name().to_str().unwrap().is_empty() {
                        let record = self.parse_record(cursor.type_().unwrap())?;
                        namespace.records.push(record);
                    }
                }
            }
            _ => {}
        }

        Ok(())
    }

    fn parse_record(&mut self, record: clang::Type) -> Result<Record, Box<dyn Error>> {
        let decl = record.declaration();
        let name = decl.name().to_str().unwrap().to_string();
        let kind = match decl.kind() {
            CursorKind::StructDecl | CursorKind::ClassDecl => RecordKind::Struct,
            CursorKind::UnionDecl => RecordKind::Union,
            _ => unreachable!(),
        };

        let mut fields = Vec::new();
        let mut bases = Vec::new();
        let mut virtual_methods = Vec::new();
        let mut inner = Namespace::new();

        decl.visit_children(|cursor| -> Result<(), Box<dyn Error>> {
            match cursor.kind() {
                // Check for UnionDecl to handle anonymous unions
                CursorKind::FieldDecl | CursorKind::UnionDecl => {
                    let type_ =
                        self.parse_type(cursor.type_().unwrap(), cursor.location(), &mut inner)?;

                    fields.push(Field {
                        name: cursor.name().to_str().unwrap().to_string(),
                        type_,
                    });
                }
                CursorKind::CxxMethod => {
                    if cursor.is_virtual() {
                        let mut arguments = Vec::new();

                        for i in 0..cursor.num_arguments().unwrap() {
                            let arg = cursor.argument(i).unwrap();

                            let arg_type =
                                self.parse_type(arg.type_().unwrap(), arg.location(), &mut inner)?;
                            arguments.push(Argument {
                                name: arg.name().to_str().unwrap().to_string(),
                                type_: arg_type,
                            });
                        }

                        let result_type = self
                            .parse_type(
                                cursor.result_type().unwrap(),
                                cursor.location(),
                                &mut inner,
                            )
                            .unwrap();

                        virtual_methods.push(Method {
                            name: cursor.name().to_str().unwrap().to_string(),
                            arguments,
                            result_type,
                        });
                    }
                }
                CursorKind::CxxBaseSpecifier => {
                    let name = cursor.type_().unwrap().declaration().name();
                    bases.push(name.to_str().unwrap().to_string());
                }
                _ => {}
            }

            Ok(())
        })?;

        decl.visit_children(|cursor| self.visit(&mut inner, cursor))?;

        Ok(Record {
            name,
            kind,
            fields,
            bases,
            virtual_methods,
            inner,
        })
    }

    fn parse_type(
        &mut self,
        type_: clang::Type,
        location: Location,
        namespace: &mut Namespace,
    ) -> Result<Type, Box<dyn Error>> {
        match type_.kind() {
            TypeKind::Void => Ok(Type::Void),
            TypeKind::Bool => Ok(Type::Bool),
            TypeKind::Char_U | TypeKind::Char_S => Ok(Type::Char),
            TypeKind::UChar => Ok(Type::UChar),
            TypeKind::UShort => Ok(Type::UShort),
            TypeKind::UInt => Ok(Type::UInt),
            TypeKind::SChar => Ok(Type::SChar),
            TypeKind::Char16 => Ok(Type::Short),
            TypeKind::WChar => Ok(Type::Unsigned(type_.size())),
            TypeKind::ULong => Ok(Type::ULong),
            TypeKind::ULongLong => Ok(Type::ULongLong),
            TypeKind::Short => Ok(Type::Short),
            TypeKind::Int => Ok(Type::Int),
            TypeKind::Long => Ok(Type::Long),
            TypeKind::LongLong => Ok(Type::LongLong),
            TypeKind::Float => Ok(Type::Float),
            TypeKind::Double => Ok(Type::Double),
            TypeKind::Pointer => {
                let pointee = type_.pointee().unwrap();
                Ok(Type::Pointer {
                    is_const: pointee.is_const(),
                    pointee: Box::new(self.parse_type(pointee, location, namespace)?),
                })
            }
            TypeKind::LValueReference => {
                let pointee = type_.pointee().unwrap();
                Ok(Type::Reference {
                    is_const: pointee.is_const(),
                    pointee: Box::new(self.parse_type(pointee, location, namespace)?),
                })
            }
            TypeKind::Record => {
                let decl = type_.declaration();
                let mut name = decl.name().to_str().unwrap().to_string();

                if name.is_empty() {
                    name = format!("__type{}", namespace.unnamed_record_counter);
                    namespace.unnamed_record_counter += 1;

                    let mut record = self.parse_record(type_)?;
                    record.name = name.clone();
                    namespace.records.push(record);
                }

                Ok(Type::Record(name))
            }
            TypeKind::Enum => {
                let decl = type_.declaration();
                Ok(Type::Typedef(decl.name().to_str().unwrap().to_string()))
            }
            TypeKind::Typedef => {
                // Skip typedef declarations that are found in system headers
                let declaration = type_.declaration();
                if declaration.is_in_system_header() {
                    let underlying_type = declaration.typedef_underlying_type().unwrap();
                    return Ok(self.parse_type(underlying_type, location, namespace)?);
                }

                let name = type_.typedef_name().unwrap().to_str().unwrap().to_string();
                Ok(Type::Typedef(name))
            }
            TypeKind::ConstantArray => {
                let size = type_.array_size().unwrap();
                let element_type =
                    self.parse_type(type_.array_element_type().unwrap(), location, namespace)?;
                Ok(Type::Array(size, Box::new(element_type)))
            }
            TypeKind::Elaborated => {
                self.parse_type(type_.named_type().unwrap(), location, namespace)
            }
            _ => Err(format!(
                "error at {location}: unhandled type kind {:?}",
                type_.kind()
            )
            .into()),
        }
    }
}
