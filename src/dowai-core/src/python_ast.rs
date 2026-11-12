//! Análise estática de modelos Python por AST (ENT-020).
//!
//! O módulo é apenas parseado: nunca importado, compilado para execução ou avaliado.
//! Extrai as referências estáticas `env.ref("x")` e `env.depends_on("x")`. Chamadas
//! `env.run(...)` são imperativas e não criam aresta no DAG (plano §5.3); são ignoradas
//! aqui. Qualquer uso de `env.ref`/`env.depends_on` que não seja literal (argumento
//! dinâmico, atributo sem chamada, `getattr(env, ...)`) falha com `DOWAI_PLAN_INVALID`
//! em vez de ser omitido.

use crate::{DowaiError, Result};
use rustpython_ast::{self as ast, Constant, Expr, Mod, Visitor};
use rustpython_parser::{parse, Mode};

const ENV_NAME: &str = "env";
const STATIC_REFERENCES: [&str; 2] = ["ref", "depends_on"];

/// Retorna os nomes de modelos declarados de forma estática por `env.ref`/`env.depends_on`,
/// na ordem em que aparecem no código-fonte.
pub fn static_env_dependencies(model_id: &str, source: &str) -> Result<Vec<String>> {
    let parsed = parse(source, Mode::Module, model_id).map_err(|error| {
        DowaiError::Model(format!(
            "{model_id}: código Python não pôde ser analisado por AST sem execução: {}",
            error.error
        ))
    })?;
    let Mod::Module(module) = parsed else {
        return Err(DowaiError::Model(format!(
            "{model_id}: módulo Python inesperado na análise AST"
        )));
    };

    let mut collector = Collector {
        model_id,
        dependencies: Vec::new(),
        error: None,
    };
    for statement in module.body {
        collector.visit_stmt(statement);
    }
    match collector.error {
        Some(error) => Err(error),
        None => Ok(collector.dependencies),
    }
}

struct Collector<'a> {
    model_id: &'a str,
    dependencies: Vec<String>,
    error: Option<DowaiError>,
}

impl Collector<'_> {
    fn fail(&mut self, message: String) {
        if self.error.is_none() {
            self.error = Some(DowaiError::Model(format!("{}: {message}", self.model_id)));
        }
    }

    fn static_reference(&mut self, method: &str, call: &ast::ExprCall) {
        let literal = match (call.args.as_slice(), call.keywords.is_empty()) {
            ([Expr::Constant(constant)], true) => match &constant.value {
                Constant::Str(name) if !name.trim().is_empty() => Some(name.clone()),
                _ => None,
            },
            _ => None,
        };
        match literal {
            Some(name) => self.dependencies.push(name),
            None => self.fail(format!(
                "Dynamic or unsupported {method}() in {}; references must use a string literal",
                self.model_id
            )),
        }
    }
}

impl Visitor for Collector<'_> {
    fn visit_expr(&mut self, node: Expr) {
        if let Expr::Call(call) = node {
            let method = env_method(&call.func);
            match method {
                Some(method) if STATIC_REFERENCES.contains(&method) => {
                    self.static_reference(method, &call)
                }
                Some(_) => {}
                None if is_getattr_on_env(&call) => self
                    .fail("dynamic attribute access on env via getattr() is not plannable".into()),
                None => self.visit_expr(*call.func),
            }
            for argument in call.args {
                self.visit_expr(argument);
            }
            for keyword in call.keywords {
                self.visit_expr(keyword.value);
            }
            return;
        }
        // Um atributo `env.ref`/`env.depends_on` fora de chamada não é resolvível estaticamente.
        if let Some(method) = env_method(&node).filter(|m| STATIC_REFERENCES.contains(m)) {
            self.fail(format!(
                "env.{method} used without a call; references must be called with a string literal"
            ));
            return;
        }
        self.generic_visit_expr(node);
    }
}

/// Nome do método quando a expressão é `env.<método>`.
fn env_method(expression: &Expr) -> Option<&str> {
    match expression {
        Expr::Attribute(attribute) => match attribute.value.as_ref() {
            Expr::Name(name) if name.id.as_str() == ENV_NAME => Some(attribute.attr.as_str()),
            _ => None,
        },
        _ => None,
    }
}

fn is_getattr_on_env(call: &ast::ExprCall) -> bool {
    matches!(call.func.as_ref(), Expr::Name(name) if name.id.as_str() == "getattr")
        && matches!(call.args.first(), Some(Expr::Name(name)) if name.id.as_str() == ENV_NAME)
}
