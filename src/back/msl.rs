/*! Metal Shading Language (MSL) backend

## Binding model

Metal's bindings are flat per resource. Since there isn't an obvious mapping
from SPIR-V's descriptor sets, we require a separate mapping provided in the options.
This mapping may have one or more resource end points for each descriptor set + index
pair.

## Outputs

In Metal, built-in shader outputs can not be nested into structures within
the output struct. If there is a structure in the outputs, and it contains any built-ins,
we move them up to the root output structure that we define ourselves.
!*/

use std::{
    fmt::{
        Display, Error as FmtError, Formatter, Write,
    },
};

use crate::{FastHashMap, FastHashSet};

#[derive(Clone, Debug, PartialEq)]
pub struct BindTarget {
    pub buffer: Option<u8>,
    pub texture: Option<u8>,
    pub sampler: Option<u8>,
}

#[derive(Clone, Debug, Hash, Eq, Ord, PartialEq, PartialOrd)]
pub struct BindSource {
    pub set: spirv::Word,
    pub binding: spirv::Word,
}

pub type BindingMap = FastHashMap<BindSource, BindTarget>;

enum ResolvedBinding {
    BuiltIn(spirv::BuiltIn),
    Attribute(spirv::Word),
    Color(spirv::Word),
    User { prefix: &'static str, index: spirv::Word },
    Resource(BindTarget),
}

#[derive(Debug)]
pub enum Error {
    Format(FmtError),
    UnsupportedExecutionModel(spirv::ExecutionModel),
    MixedExecutionModels(crate::Token<crate::Function>),
    MissingBinding(crate::Token<crate::GlobalVariable>),
    MissingBindTarget(BindSource),
    BadName(String),
}

impl From<FmtError> for Error {
    fn from(e: FmtError) -> Self {
        Error::Format(e)
    }
}

#[derive(Clone, Copy, Debug)]
enum LocationMode {
    VertexInput,
    FragmentOutput,
    Intermediate,
}

pub struct Options<'a> {
    pub binding_map: &'a BindingMap,
}

impl Options<'_> {
    fn resolve_binding(&self, binding: &crate::Binding, mode: LocationMode) -> Result<ResolvedBinding, Error> {
        match *binding {
            crate::Binding::BuiltIn(built_in) => Ok(ResolvedBinding::BuiltIn(built_in)),
            crate::Binding::Location(index) => Ok(match mode {
                LocationMode::VertexInput => ResolvedBinding::Attribute(index),
                LocationMode::FragmentOutput => ResolvedBinding::Color(index),
                LocationMode::Intermediate => ResolvedBinding::User {
                    prefix: "loc",
                    index,
                },
            }),
            crate::Binding::Descriptor { set, binding } => {
                let source = BindSource { set, binding };
                self.binding_map
                    .get(&source)
                    .cloned()
                    .map(ResolvedBinding::Resource)
                    .ok_or(Error::MissingBindTarget(source))

            }
        }
    }
}


trait Indexed {
    const CLASS: &'static str;
    const PREFIX: bool = false;
    fn id(&self) -> usize;
}

impl Indexed for crate::Token<crate::PointerDeclaration> {
    const CLASS: &'static str = "Pointer";
    fn id(&self) -> usize { self.index() }
}
impl Indexed for crate::Token<crate::ArrayDeclaration> {
    const CLASS: &'static str = "Array";
    fn id(&self) -> usize { self.index() }
}
impl Indexed for crate::Token<crate::StructDeclaration> {
    const CLASS: &'static str = "Struct";
    fn id(&self) -> usize { self.index() }
}
impl Indexed for crate::Token<crate::GlobalVariable> {
    const CLASS: &'static str = "global";
    fn id(&self) -> usize { self.index() }
}
impl Indexed for crate::Token<crate::Function> {
    const CLASS: &'static str = "function";
    fn id(&self) -> usize { self.index() }
}

struct MemberIndex(usize);
impl Indexed for MemberIndex {
    const CLASS: &'static str = "field";
    fn id(&self) -> usize { self.0 }
}
struct ParameterIndex(usize);
impl Indexed for ParameterIndex {
    const CLASS: &'static str = "param";
    fn id(&self) -> usize { self.0 }
}
struct InputStructIndex(crate::Token<crate::Function>);
impl Indexed for InputStructIndex {
    const CLASS: &'static str = "Input";
    const PREFIX: bool = true;
    fn id(&self) -> usize { self.0.index() }
}
struct OutputStructIndex(crate::Token<crate::Function>);
impl Indexed for OutputStructIndex {
    const CLASS: &'static str = "Output";
    const PREFIX: bool = true;
    fn id(&self) -> usize { self.0.index() }
}

enum NameSource<'a> {
    Custom { name: &'a str, prefix: bool },
    Index(usize),
}

const RESERVED_NAMES: &[&str] = &[
    "main",
];

struct Name<'a> {
    class: &'static str,
    source: NameSource<'a>,
}
impl Display for Name<'_> {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> Result<(), FmtError> {
        match self.source {
            NameSource::Custom { name, prefix: false } if RESERVED_NAMES.contains(&name) => {
                write!(formatter, "{}_", name)
            }
            NameSource::Custom { name, prefix: false } => formatter.write_str(name),
            NameSource::Custom { name, prefix: true } => {
                let (head, tail) = name.split_at(1);
                write!(formatter, "{}{}{}", self.class, head.to_uppercase(), tail)
            }
            NameSource::Index(index) => write!(formatter, "{}{}", self.class, index),
        }
    }
}
impl<I: Indexed> From<I> for Name<'_> {
    fn from(index: I) -> Self {
        Name {
            class: I::CLASS,
            source: NameSource::Index(index.id()),
        }
    }
}

trait AsName {
    fn or_index<I: Indexed>(&self, index: I) -> Name;
}
impl AsName for Option<String> {
    fn or_index<I: Indexed>(&self, index: I) -> Name {
        Name {
            class: I::CLASS,
            source: match *self {
                Some(ref name) if !name.is_empty() => NameSource::Custom { name, prefix: I::PREFIX },
                _ => NameSource::Index(index.id()),
            },
        }
    }
}

struct Starred<T>(T);
impl<T: Display> Display for Starred<T> {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> Result<(), FmtError> {
        write!(formatter, "*{}", self.0)
    }
}

struct TypedVar<'a, T>(&'a crate::Type, &'a T, &'a crate::ComplexTypes);
impl<T: Display> Display for TypedVar<'_, T> {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> Result<(), FmtError> {
        match *self.0 {
            crate::Type::Void => {
                write!(formatter, "void {}", self.1)
            },
            crate::Type::Scalar { kind, .. } => {
                write!(formatter, "{} {}", scalar_kind_string(kind), self.1)
            },
            crate::Type::Vector { size, kind, .. } => {
                write!(formatter, "{}{} {}", scalar_kind_string(kind), vector_size_string(size), self.1)
            },
            crate::Type::Matrix { columns, rows, kind, .. } => {
                write!(formatter, "{}{}x{} {}", scalar_kind_string(kind), vector_size_string(columns), vector_size_string(rows), self.1)
            }
            crate::Type::Pointer(token) => {
                let decl = &self.2.pointers[token];
                write!(formatter, "{} {}", decl.name.or_index(token), self.1)
            }
            crate::Type::Array(token) => {
                let decl = &self.2.arrays[token];
                write!(formatter, "{} {}", decl.name.or_index(token), self.1)
            }
            crate::Type::Struct(token) => {
                let decl = &self.2.structs[token];
                write!(formatter, "{} {}", decl.name.or_index(token), self.1)
            }
        }
    }
}

struct TypedGlobalVariable<'a> {
    module: &'a crate::Module,
    token: crate::Token<crate::GlobalVariable>,
}
impl Display for TypedGlobalVariable<'_> {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> Result<(), FmtError> {
        let var = &self.module.global_variables[self.token];
        let name = var.name.or_index(self.token);
        match var.ty {
            crate::Type::Pointer(token) => {
                let decl = &self.module.complex_types.pointers[token];
                let ty = match decl.class {
                    spirv::StorageClass::Input |
                    spirv::StorageClass::Output => &decl.base,
                    _ => &var.ty
                };
                let tv = TypedVar(ty, &name, &self.module.complex_types);
                write!(formatter, "{}", tv)
            }
            _ => panic!("Unexpected global type {:?}", var.ty),
        }
    }
}

impl Display for ResolvedBinding {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> Result<(), FmtError> {
        match *self {
            ResolvedBinding::BuiltIn(built_in) => {
                let name = match built_in {
                    spirv::BuiltIn::ClipDistance => "clip_distance",
                    spirv::BuiltIn::PointSize => "point_size",
                    spirv::BuiltIn::Position => "position",
                    _ => panic!("Built in {:?} is not implemented", built_in),
                };
                formatter.write_str(name)
            }
            ResolvedBinding::Attribute(index) => {
                write!(formatter, "attribute({})", index)
            }
            ResolvedBinding::Color(index) => {
                write!(formatter, "color({})", index)
            }
            ResolvedBinding::User { prefix, index } => {
                write!(formatter, "user({}{})", prefix, index)
            }
            ResolvedBinding::Resource(ref target) => {
                if let Some(id) = target.buffer {
                    write!(formatter, "buffer({})", id)
                } else if let Some(id) = target.texture {
                    write!(formatter, "buffer({})", id)
                } else if let Some(id) = target.sampler {
                    write!(formatter, "sampler({})", id)
                } else {
                    unimplemented!()
                }
            }
        }
    }
}

pub struct Writer<W> {
    out: W,
}

fn scalar_kind_string(kind: crate::ScalarKind) -> &'static str {
    match kind {
        crate::ScalarKind::Float => "float",
        crate::ScalarKind::Sint => "signed int",
        crate::ScalarKind::Uint => "unsigned int",
    }
}

fn vector_size_string(size: crate::VectorSize) -> &'static str {
    match size {
        crate::VectorSize::Bi => "2",
        crate::VectorSize::Tri => "3",
        crate::VectorSize::Quad => "4",
    }
}

const NAME_INPUT: &'static str = "input";
const NAME_OUTPUT: &'static str = "output";

impl<W: Write> Writer<W> {
    pub fn write(&mut self, module: &crate::Module, options: Options) -> Result<(), Error> {
        writeln!(self.out, "#include <metal_stdlib>")?;
        writeln!(self.out, "#include <simd/simd.h>")?;
        writeln!(self.out, "using namespace metal;")?;

        // write down complex types
        writeln!(self.out, "")?;
        for (token, decl) in module.complex_types.pointers.iter() {
            let name = Starred(decl.name.or_index(token));
            // Input and output are always provided as pointers,
            // but we are only interested in the contents.
            //TODO: consider resolving this at the root
            let class = match decl.class {
                spirv::StorageClass::Input |
                spirv::StorageClass::Output => continue,
                spirv::StorageClass::Uniform => "constant",
                other => {
                    log::warn!("Unexpected pointer class {:?}", other);
                    ""
                }
            };
            if let crate::Type::Struct(st) = decl.base {
                let var = module.complex_types.structs[st].name.or_index(st);
                writeln!(self.out, "struct {}; //forward decl", var)?;
            }
            let tv = TypedVar(&decl.base, &name, &module.complex_types);
            writeln!(self.out, "typedef {} {};", class, tv)?;
        }
        for (token, decl) in module.complex_types.arrays.iter() {
            let name = decl.name.or_index(token);
            let tv = TypedVar(&decl.base, &name, &module.complex_types);
            writeln!(self.out, "typedef {}[{}];", tv, decl.length)?;
        }
        for (token, decl) in module.complex_types.structs.iter() {
            writeln!(self.out, "struct {} {{", decl.name.or_index(token))?;
            for (index, member) in decl.members.iter().enumerate() {
                let name = member.name.or_index(MemberIndex(index));
                let tv = TypedVar(&member.ty, &name, &module.complex_types);
                write!(self.out, "\t{}", tv)?;
                if let Some(ref binding) = member.binding {
                    let resolved = options.resolve_binding(binding, LocationMode::Intermediate)?;
                    write!(self.out, " [[{}]]", resolved)?;
                }
                writeln!(self.out, ";")?;
            }
            writeln!(self.out, "}};")?;
        }

        // write down functions
        let mut uniforms_used = FastHashSet::default();
        writeln!(self.out, "")?;
        for (fun_token, fun) in module.functions.iter() {
            let mut exec_model = None;
            let mut var_inputs = FastHashSet::default();
            let mut var_outputs = FastHashSet::default();
            for ep in module.entry_points.iter() {
                if ep.function == fun_token {
                    var_inputs.extend(ep.inputs.iter().cloned());
                    var_outputs.extend(ep.outputs.iter().cloned());
                    if exec_model.is_some() {
                        if exec_model != Some(ep.exec_model) {
                            return Err(Error::MixedExecutionModels(fun_token));
                        }
                    } else {
                        exec_model = Some(ep.exec_model);
                    }
                }
            }
            let input_name = fun.name.or_index(InputStructIndex(fun_token));
            let output_name = fun.name.or_index(OutputStructIndex(fun_token));
            if let Some(em) = exec_model {
                writeln!(self.out, "struct {} {{", input_name)?;
                let (em_str, in_mode, out_mode) = match em {
                    spirv::ExecutionModel::Vertex => ("vertex", LocationMode::VertexInput, LocationMode::Intermediate),
                    spirv::ExecutionModel::Fragment => ("fragment", LocationMode::Intermediate, LocationMode::FragmentOutput),
                    spirv::ExecutionModel::GLCompute => ("compute", LocationMode::Intermediate, LocationMode::Intermediate),
                    _ => return Err(Error::UnsupportedExecutionModel(em)),
                };
                for &token in var_inputs.iter() {
                    let var = &module.global_variables[token];
                    let tyvar = TypedGlobalVariable { module, token };
                    write!(self.out, "\t{}", tyvar)?;
                    if let Some(ref binding) = var.binding {
                        let resolved = options.resolve_binding(binding, in_mode)?;
                        write!(self.out, " [[{}]]", resolved)?;
                    }
                    writeln!(self.out, ";")?;
                }
                writeln!(self.out, "}};")?;
                writeln!(self.out, "struct {} {{", output_name)?;
                for &token in var_outputs.iter() {
                    let var = &module.global_variables[token];
                    // if it's a struct, lift all the built-in contents up to the root
                    if let crate::Type::Pointer(pt) = var.ty {
                        if let crate::Type::Struct(st) = module.complex_types.pointers[pt].base {
                            let sd = &module.complex_types.structs[st];
                            for (index, member) in sd.members.iter().enumerate() {
                                let name = member.name.or_index(MemberIndex(index));
                                let tv = TypedVar(&member.ty, &name, &module.complex_types);
                                let binding = member.binding
                                    .as_ref()
                                    .ok_or(Error::MissingBinding(token))?;
                                let resolved = options.resolve_binding(binding, out_mode)?;
                                writeln!(self.out, "\t{} [[{}]];", tv, resolved)?;
                            }
                            continue
                        }
                    }
                    let tyvar = TypedGlobalVariable { module, token };
                    write!(self.out, "\t{}", tyvar)?;
                    if let Some(ref binding) = var.binding {
                        let resolved = options.resolve_binding(binding, out_mode)?;
                        write!(self.out, " [[{}]]", resolved)?;
                    }
                    writeln!(self.out, ";")?;
                }
                writeln!(self.out, "}};")?;
                write!(self.out, "{} ", em_str)?;
            }

            let fun_name = fun.name.or_index(fun_token);
            if exec_model.is_some() {
                writeln!(self.out, "{} {}(", output_name, fun_name)?;
                writeln!(self.out, "\t{} {} [[stage_in]],", input_name, NAME_INPUT)?;
            } else {
                let fun_tv = TypedVar(&fun.return_type, &fun_name, &module.complex_types);
                writeln!(self.out, "{}(", fun_tv)?;
                for (index, ty) in fun.parameter_types.iter().enumerate() {
                    let name = Name::from(ParameterIndex(index));
                    let tv = TypedVar(ty, &name, &module.complex_types);
                    writeln!(self.out, "\t{},", tv)?;
                }
            }
            for (_, expr) in fun.expressions.iter() {
                if let crate::Expression::GlobalVariable(token) = *expr {
                    let var = &module.global_variables[token];
                    if var.class == spirv::StorageClass::Uniform && !uniforms_used.contains(&token) {
                        uniforms_used.insert(token);
                        let var = TypedGlobalVariable { module, token };
                        writeln!(self.out, "\t{},", var)?;
                    }
                }
            }
            // add an extra parameter to make Metal happy about the comma
            match exec_model {
                Some(spirv::ExecutionModel::Vertex) => {
                    writeln!(self.out, "\tunsigned _dummy [[vertex_id]]")?;
                }
                Some(spirv::ExecutionModel::Fragment) => {
                    writeln!(self.out, "\tbool _dummy [[front_facing]]")?;
                }
                Some(spirv::ExecutionModel::GLCompute) => {
                    writeln!(self.out, "\tunsigned _dummy [[threads_per_grid]]")?;
                }
                _ => {
                    writeln!(self.out, "\tint _dummy")?;
                }
            }
            writeln!(self.out, ") {{")?;
            writeln!(self.out, "\t{} {};", output_name, NAME_OUTPUT)?;
            writeln!(self.out, "\treturn {};", NAME_OUTPUT)?;
            writeln!(self.out, "}}")?;
        }

        Ok(())
    }
}

pub fn write_string(module: &crate::Module, options: Options) -> Result<String, Error> {
    let mut w = Writer { out: String::new() };
    w.write(module, options)?;
    Ok(w.out)
}