use std::ops;

use quickscope::ScopeMap;

use crate::{Symbol, arena::{Index, Interner, Arena}, ast::{Ast, IntegerWidth, PathIter, SymbolPath}, util::files::FileId};

pub type TypeId = Index<Type>;
pub type FunId = Index<Function>;
pub type ModId = Index<SparkModule>;
pub type DefId = Index<SparkDef>;

/// Structure containing arenas holding all function definitions, 
/// types, etc.
#[derive(Clone, Debug)]
pub struct SparkCtx {
    types: Interner<Type>,
    modules: Arena<SparkModule>,
    funs: Arena<Function>,
    root_module: ModId,
}

static mut COUNT: usize = 0;

impl SparkCtx {
    
    /// Create a new module with the given name and return an ID for the created
    /// module
    pub fn new_module(&mut self, name: Symbol, file: FileId) -> ModId {
        self.modules.insert_with(|id| SparkModule {
            id,
            file,
            name,
            defs: ScopeMap::new(),
        })
    }
    
    /// Create a type using the given type data and return the ID of the created
    /// type
    pub fn new_type(&mut self, data: TypeData) -> TypeId {
        self.types.insert_with(|id| {
            Type {
                id,
                data,
            }
        })
    }
    
    /// Create a new invalid type with a unique type ID for forward references
    pub fn new_empty_type(&mut self) -> TypeId {
        unsafe { 
            COUNT += 1;
            self.new_type(TypeData::Invalid(COUNT)) 
        }
    }
    
    /// Create a new function and return the ID of the created function
    pub fn new_fun(&mut self, name: Symbol, ty: FunctionType, arg_names: Vec<Option<Symbol>>) -> FunId {
        self.funs.insert_with(|id| Function {
            id,
            name,
            ty,
            arg_names,
            body: None,
        })
    }
    
    /// Get the name of a definition
    pub fn get_def_name(&self, def: SparkDef) -> Symbol {
        match def {
            SparkDef::TypeDef(ty) => unimplemented!(),
            SparkDef::FunDef(fun) => self.funs[fun].name,
            SparkDef::ModDef(module) => self.modules[module].name,
        }
    }
    
    /// Get a definition by path from the given module, returns the symbol that is unresolved if
    /// error occurs
    pub fn get_def(&self, module: ModId, path: &SymbolPath) -> Result<SparkDef, Symbol> {
        let parts = path.iter();
        self.get_def_impl(module, parts)
    }

    fn get_def_impl(&self, module: ModId, mut parts: PathIter<'_>) -> Result<SparkDef, Symbol> {
        if parts.len() == 1 {
            println!("get_def_impl works!");
            let name = parts.next().unwrap();
            let def = self.modules[module].defs.get(&name);
            def.copied().ok_or(name)
        } else {
            let name = parts.next().expect("invariant in get_def_impl");
            let def = self[module].defs.get(&name);
            if let Some(def) = def {
                if let SparkDef::ModDef(mod_id) = def {
                    return self.get_def_impl(*mod_id, parts);
                } else if parts.is_final() {
                    if let SparkDef::TypeDef(ty) = def {
                        unimplemented!("Functions associated with types not implemented");
                    }
                }

                Err(name)
            } else {
                Err(name)
            }
            
        }
    }

    pub const I8:  TypeId = unsafe { TypeId::from_raw(0) };
    pub const I16: TypeId = unsafe { TypeId::from_raw(1) };
    pub const I32: TypeId = unsafe { TypeId::from_raw(2) };
    pub const I64: TypeId = unsafe { TypeId::from_raw(3) };

    pub const U8:  TypeId = unsafe { TypeId::from_raw(4) };
    pub const U16: TypeId = unsafe { TypeId::from_raw(5) };
    pub const U32: TypeId = unsafe { TypeId::from_raw(6) };
    pub const U64: TypeId = unsafe { TypeId::from_raw(7) };

    pub const F32:  TypeId = unsafe { TypeId::from_raw(8) };
    pub const F64:  TypeId = unsafe { TypeId::from_raw(9) };
    pub const BOOL: TypeId = unsafe { TypeId::from_raw(10) };
    pub const UNIT: TypeId = unsafe { TypeId::from_raw(11) };

    pub fn new(root_file: FileId) -> Self {
        let mut types = Interner::new();
        let mut modules = Arena::new();
        let root_module = modules.insert_with(|id| SparkModule { id, file: root_file, name: Symbol::from("root"), defs: ScopeMap::new()});

        types.insert_with(|id| Type { id, data: TypeData::Integer { width: IntegerWidth::Eight, signed: true}});
        types.insert_with(|id| Type { id, data: TypeData::Integer { width: IntegerWidth::Sixteen, signed: true}});
        types.insert_with(|id| Type { id, data: TypeData::Integer { width: IntegerWidth::ThirtyTwo, signed: true}});
        types.insert_with(|id| Type { id, data: TypeData::Integer { width: IntegerWidth::SixtyFour, signed: true}});
 
        types.insert_with(|id| Type { id, data: TypeData::Integer { width: IntegerWidth::Eight, signed: false}});
        types.insert_with(|id| Type { id, data: TypeData::Integer { width: IntegerWidth::Sixteen, signed: false}});
        types.insert_with(|id| Type { id, data: TypeData::Integer { width: IntegerWidth::ThirtyTwo, signed: false}});
        types.insert_with(|id| Type { id, data: TypeData::Integer { width: IntegerWidth::SixtyFour, signed: false}});
 
        types.insert_with(|id| Type { id, data: TypeData::Float { doublewide: false }});
        types.insert_with(|id| Type { id, data: TypeData::Float { doublewide: true }});
        types.insert_with(|id| Type { id, data: TypeData::Bool });
        types.insert_with(|id| Type { id, data: TypeData::Unit });

        Self {
            types,
            modules,
            root_module,
            funs: Arena::new(),
        }
    }
}

/// Structure containing type data plus a type ID that can be used to refer to the
/// type
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Type {
    pub data: TypeData,
    pub id: TypeId,
}

/// Function containing an entry basic block and argument data
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Function {
    pub id: FunId,
    pub name: Symbol,
    pub ty: FunctionType,
    pub arg_names: Vec<Option<Symbol>>,
    pub body: Option<Vec<Ast<TypeId>>>,
}

/// A single type, either user-defined or predefined
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum TypeData {
    Integer {
        signed: bool,
        width: IntegerWidth,
    },
    Float {
        doublewide: bool,
    },
    Bool,
    Unit,
    Pointer(TypeId),
    Array {
        element: TypeId,
        len: u64,
    },
    Tuple(Vec<TypeId>),
    Struct {
        //Prevents interning from seeing two structure types as different
        name: Option<Symbol>,
        fields: Vec<(TypeId, Symbol)>,
    },
    Enum {
        name: Option<Symbol>,
        parts: Vec<TypeId>,
    },
    Alias(TypeId),
    Function(FunctionType),
    /// For internal compiler use only
    Invalid(usize),
}

/// A function's type including argument types, return type, and flags
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct FunctionType {
    pub return_ty: TypeId,
    pub args: Vec<TypeId>,
}


/// Structure holding all definitions contained in a single module
#[derive(Clone,)]
pub struct SparkModule {
    pub id: ModId,
    pub file: FileId,
    pub name: Symbol,
    pub defs: ScopeMap<Symbol, SparkDef>,
}

impl std::fmt::Debug for SparkModule {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut s = f.debug_struct("SparkModule");
        s.field("id", &self.id);
        s.field("file", &self.file);
        s.field("name", &self.name.to_string());
        for (name, def) in self.defs.iter() {
            s.field(name.as_str(), &def);
        }
        s.finish()
    }
}

/// A single definition in the 
#[derive(Clone, Copy, Debug)]
pub enum SparkDef {
    TypeDef(TypeId),
    FunDef(FunId),
    ModDef(ModId),
}

impl ops::Index<TypeId> for SparkCtx {
    type Output = Type;
    fn index(&self, index: TypeId) -> &Self::Output {
        self.types.get(index)
    }
}
impl ops::IndexMut<TypeId> for SparkCtx {
    fn index_mut(&mut self, index: TypeId) -> &mut Self::Output {
        self.types.get_mut(index)
    }
}
impl ops::Index<ModId> for SparkCtx {
    type Output = SparkModule;
    fn index(&self, index: ModId) -> &Self::Output {
        self.modules.get(index)
    }
}
impl ops::IndexMut<ModId> for SparkCtx {
    fn index_mut(&mut self, index: ModId) -> &mut Self::Output {
        self.modules.get_mut(index)
    }
}
impl ops::Index<FunId> for SparkCtx {
    type Output = Function;
    fn index(&self, index: FunId) -> &Self::Output {
        self.funs.get(index)
    }
}
impl ops::IndexMut<FunId> for SparkCtx {
    fn index_mut(&mut self, index: FunId) -> &mut Self::Output {
        self.funs.get_mut(index)
    }
}