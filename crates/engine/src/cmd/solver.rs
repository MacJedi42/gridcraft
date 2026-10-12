//! Solver (Data › Analysis › Solver): finds values for variable cells that maximise, minimise
//! or hit a value in an objective cell, subject to constraints.
//!
//! The workbook is the model: every evaluation sets the variable cells on a copy of the
//! workbook and recalculates their dependents ([`Trial`]), so any formula works. Two methods:
//!
//! - **Simplex LP**: the model is linearised from `n + 1` evaluations and checked for linearity
//!   at one more point; then a dense two-phase simplex (Dantzig's rule, Bland's rule when it
//!   stalls) solves it exactly. Integer and binary constraints use depth-first branch and bound.
//! - **GRG Nonlinear** (the name Excel's dialog uses): GridCraft's method is *not* a generalised
//!   reduced gradient. It is an augmented-Lagrangian method (Powell–Hestenes–Rockafellar) whose
//!   inner problems are solved by projected gradient descent with Barzilai–Borwein steps and an
//!   Armijo line search, on central-difference gradients. Variable bounds are kept by
//!   projection. It finds a local optimum of a smooth model; integer constraints use a small
//!   branch and bound on top.
//!
//! Work is bounded by an evaluation budget (no clock: `Instant` traps on wasm). The model is
//! saved as hidden sheet-scoped names (`solver_opt`, `solver_adj`, `solver_lhs1`…), so it
//! travels with the workbook through XLSX.

use gridcraft_core::{CellRef, RangeRef, Value};
use gridcraft_model::{Cell, DefinedName, Workbook};
use serde_json::{Value as Json, json};

use super::whatif::Trial;
use super::*;

const CMD: &str = "data.solver";
/// Excel's limit on decision variables.
const MAX_VARS: usize = 200;
/// Most scalar constraints (after expanding ranges).
const MAX_CONSTRAINTS: usize = 1000;
const DEFAULT_EVALS: u64 = 20_000;
const MAX_EVALS: u64 = 200_000;
/// Most simplex pivots × tableau cells per solve.
const LP_WORK: u64 = 2_000_000_000;
const MAX_NODES_LP: usize = 10_000;
const MAX_NODES_NLP: usize = 200;
/// Constraint precision (Excel's default 0.000001).
const PRECISION: f64 = 1e-6;
const EPS: f64 = 1e-9;

pub fn specs() -> Vec<CommandSpec> {
    vec![cmd!(
        "data.solver",
        "Solver…",
        ["Data", "Analysis"],
        None,
        "{objective: \"B10\" (formula cell), goal?: max|min|value (default max), value?: number (for goal value), variables: \"B2:B4,D1\" (≤200 constant cells, same sheet), constraints?: [\"B5 <= 10\", \"B2:B4 >= C2:C4\", \"B2 int\", \"B3 bin\", {lhs: \"B6\", op: \"=\", rhs: 3 | \"C6\"}], nonNegative?: bool (default true: variables without a lower bound get 0), method?: grg|simplex (default grg), keep?: bool (default true; false restores the original values), saveModel?: bool (default true: hidden solver_* names), maxEvaluations?: 20000, integerOptimality?: percent (default 0), saved?: bool (solve the sheet's saved model; other params override)} → {status: optimal|converged|iterationLimit|infeasible|unbounded|notLinear, message, objective, values: {cell: value}, evaluations, changed}. No `objective` → {model: the saved model} (the menu opens the dialog).",
        has_doc,
        run
    )]
}

// ---------------------------------------------------------------- model

#[derive(Clone, Copy, Debug, PartialEq)]
enum Rel {
    Le,
    Eq,
    Ge,
    Int,
    Bin,
}

impl Rel {
    fn parse(s: &str) -> Option<Rel> {
        match s.trim().to_ascii_lowercase().as_str() {
            "<=" | "≤" | "=<" | "le" => Some(Rel::Le),
            ">=" | "≥" | "=>" | "ge" => Some(Rel::Ge),
            "=" | "==" | "eq" => Some(Rel::Eq),
            "int" | "integer" => Some(Rel::Int),
            "bin" | "binary" => Some(Rel::Bin),
            _ => None,
        }
    }
    fn text(self) -> &'static str {
        match self {
            Rel::Le => "<=",
            Rel::Eq => "=",
            Rel::Ge => ">=",
            Rel::Int => "int",
            Rel::Bin => "bin",
        }
    }
    /// The relation code of the saved model (`solver_relN`), as in Solver's VBA `SolverAdd`.
    fn code(self) -> u8 {
        match self {
            Rel::Le => 1,
            Rel::Eq => 2,
            Rel::Ge => 3,
            Rel::Int => 4,
            Rel::Bin => 5,
        }
    }
    fn from_code(c: &str) -> Option<Rel> {
        match c.trim() {
            "1" => Some(Rel::Le),
            "2" => Some(Rel::Eq),
            "3" => Some(Rel::Ge),
            "4" => Some(Rel::Int),
            "5" => Some(Rel::Bin),
            _ => None,
        }
    }
}

#[derive(Clone, Debug)]
enum Rhs {
    None,
    Const(f64),
    Cells(Vec<CellRef>),
}

#[derive(Clone, Debug)]
struct Constraint {
    lhs: Vec<CellRef>,
    lhs_text: String,
    rel: Rel,
    rhs: Rhs,
    rhs_text: String,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Goal {
    Max,
    Min,
    Value(f64),
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Method {
    Simplex,
    Grg,
}

#[derive(Clone, Debug)]
struct Model {
    sheet: usize,
    objective: CellRef,
    goal: Goal,
    vars: Vec<CellRef>,
    vars_text: String,
    cons: Vec<Constraint>,
    non_negative: bool,
    method: Method,
}

/// Cells of `"B2:B4,D1"` on one sheet (row-major per area, in order), at most `cap`.
fn cells_of(wb: &Workbook, t: &str, default_sheet: usize, cap: usize) -> Result<(usize, Vec<CellRef>)> {
    let mut sheet = None;
    let mut out = Vec::new();
    for part in t.split(',').map(str::trim).filter(|x| !x.is_empty()) {
        let (sh, body) = split_sheet(part);
        let si = match sh {
            Some(n) => wb.sheet_index(&n).ok_or_else(|| bad(CMD, format!("no sheet named `{n}`")))?,
            None => default_sheet,
        };
        if *sheet.get_or_insert(si) != si {
            return Err(bad(CMD, "all cells of a reference must be on one sheet"));
        }
        let r = RangeRef::parse(body.trim()).ok_or_else(|| bad(CMD, format!("not a reference: `{part}`")))?;
        if r.count() > cap as u64 || out.len() + r.count() as usize > cap {
            return Err(bad(CMD, format!("too many cells (at most {cap})")));
        }
        out.extend(r.iter());
    }
    if out.is_empty() {
        return Err(bad(CMD, format!("not a reference: `{t}`")));
    }
    Ok((sheet.unwrap_or(default_sheet), out))
}

fn num_text(t: &str) -> Option<f64> {
    gridcraft_core::parse::parse_number_text(t.trim()).filter(|v| v.is_finite())
}

/// Splits `"B5 <= 10"` / `"B2 int"` into (lhs, relation, rhs).
fn split_constraint(t: &str) -> Option<(String, Rel, String)> {
    for op in ["<=", ">=", "=<", "=>", "≤", "≥", "=="] {
        if let Some((a, b)) = t.split_once(op) {
            return Some((a.trim().into(), Rel::parse(op)?, b.trim().into()));
        }
    }
    if let Some((a, b)) = t.split_once('=') {
        return Some((a.trim().into(), Rel::Eq, b.trim().into()));
    }
    let lower = t.trim().to_ascii_lowercase();
    for (suffix, rel) in [("integer", Rel::Int), ("binary", Rel::Bin), ("int", Rel::Int), ("bin", Rel::Bin)] {
        if let Some(a) = lower.strip_suffix(suffix) {
            let a = a.trim();
            return Some((t.trim().get(..a.len())?.trim().to_string(), rel, String::new()));
        }
    }
    None
}

fn parse_constraint(wb: &Workbook, sheet: usize, c: &Json) -> Result<Constraint> {
    let (lhs_text, rel, rhs_json) = match c {
        Json::String(t) => {
            let (l, r, rhs) = split_constraint(t).ok_or_else(|| bad(CMD, format!("not a constraint: `{t}`")))?;
            (l, r, Json::String(rhs))
        }
        Json::Object(_) => {
            let lhs = str_param(c, "lhs").or_else(|| str_param(c, "cell")).or_else(|| str_param(c, "range")).unwrap_or("").to_string();
            let op = str_param(c, "op").or_else(|| str_param(c, "relation")).unwrap_or("");
            let rel = Rel::parse(op).ok_or_else(|| bad(CMD, format!("unknown constraint relation `{op}`")))?;
            (lhs, rel, c.get("rhs").or_else(|| c.get("value")).cloned().unwrap_or(Json::Null))
        }
        _ => return Err(bad(CMD, "a constraint is a string like \"B5 <= 10\" or {lhs, op, rhs}")),
    };
    let (lsi, lhs) = cells_of(wb, &lhs_text, sheet, MAX_CONSTRAINTS)?;
    if lsi != sheet {
        return Err(EngineError::Other("Constraint cells must be on the objective cell's sheet.".into()));
    }
    let (rhs, rhs_text) = match rel {
        Rel::Int | Rel::Bin => (Rhs::None, String::new()),
        _ => match &rhs_json {
            Json::Number(n) => {
                let v = n.as_f64().filter(|v| v.is_finite()).ok_or_else(|| bad(CMD, "the constraint's right side must be a number"))?;
                (Rhs::Const(v), v.to_string())
            }
            Json::String(t) => {
                let t = t.trim().trim_start_matches('=').trim();
                if let Some(v) = num_text(t) {
                    (Rhs::Const(v), t.to_string())
                } else {
                    let (rsi, cells) = cells_of(wb, t, sheet, MAX_CONSTRAINTS)?;
                    if rsi != sheet {
                        return Err(EngineError::Other("Constraint cells must be on the objective cell's sheet.".into()));
                    }
                    if cells.len() != 1 && cells.len() != lhs.len() {
                        return Err(EngineError::Other("The constraint's two sides must be the same size (or the right side one cell).".into()));
                    }
                    (Rhs::Cells(cells), t.to_string())
                }
            }
            _ => return Err(bad(CMD, "missing the constraint's right side")),
        },
    };
    Ok(Constraint { lhs, lhs_text, rel, rhs, rhs_text })
}

fn parse_model(s: &Session, p: &Json) -> Result<Model> {
    let d = s.doc()?;
    let wb = &d.wb;
    let obj_text = str_param(p, "objective").or_else(|| str_param(p, "setObjective")).or_else(|| str_param(p, "set")).unwrap_or("");
    let (sheet, obj) = cells_of(wb, obj_text, wb.active_sheet, 1).map_err(|_| EngineError::Other("The objective must be a single cell.".into()))?;
    let objective = *obj.first().ok_or_else(|| EngineError::Other("The objective must be a single cell.".into()))?;
    let sh = wb.sheet(sheet).ok_or(EngineError::NoDocument)?;
    if sh.cell(objective).is_none_or(|c| c.formula.is_none()) {
        return Err(EngineError::Other("The objective cell must contain a formula.".into()));
    }
    let goal = match str_param(p, "goal").or_else(|| str_param(p, "to")).unwrap_or("max").to_ascii_lowercase().as_str() {
        "max" | "maximize" | "maximum" => Goal::Max,
        "min" | "minimize" | "minimum" => Goal::Min,
        "value" | "valueof" | "value of" => {
            let v = f64_param(p, "value")
                .or_else(|| str_param(p, "value").and_then(num_text))
                .filter(|v| v.is_finite())
                .ok_or_else(|| bad(CMD, "goal `value` needs a number in `value`"))?;
            Goal::Value(v)
        }
        g => return Err(bad(CMD, format!("unknown goal `{g}` (max, min or value)"))),
    };
    let vars_text = str_param(p, "variables").or_else(|| str_param(p, "changing")).or_else(|| str_param(p, "byChanging")).unwrap_or("").to_string();
    let (vsi, mut vars) = cells_of(wb, &vars_text, sheet, MAX_VARS).map_err(|e| match e {
        EngineError::BadParams { msg, .. } if msg.starts_with("too many") => {
            EngineError::Other(format!("Too many variable cells (at most {MAX_VARS})."))
        }
        _ => EngineError::Other("The variable cells must be a reference like B2:B4.".into()),
    })?;
    if vsi != sheet {
        return Err(EngineError::Other("The variable cells must be on the objective cell's sheet.".into()));
    }
    let mut seen = std::collections::HashSet::new();
    vars.retain(|c| seen.insert(*c));
    if vars.iter().any(|c| sh.cell(*c).is_some_and(|c| c.formula.is_some())) {
        return Err(EngineError::Other("Variable cells must hold constants, not formulas.".into()));
    }
    let mut cons = Vec::new();
    let mut scalar = 0usize;
    if let Some(list) = p.get("constraints") {
        let list = list.as_array().ok_or_else(|| bad(CMD, "`constraints` must be an array"))?;
        if list.len() > MAX_CONSTRAINTS {
            return Err(EngineError::Other(format!("Too many constraints (at most {MAX_CONSTRAINTS}).")));
        }
        for c in list {
            let c = parse_constraint(wb, sheet, c)?;
            scalar = scalar.saturating_add(c.lhs.len());
            if scalar > MAX_CONSTRAINTS {
                return Err(EngineError::Other(format!("Too many constraints (at most {MAX_CONSTRAINTS} cells).")));
            }
            if matches!(c.rel, Rel::Int | Rel::Bin) && c.lhs.iter().any(|x| !vars.contains(x)) {
                return Err(EngineError::Other("Integer and binary constraints must be on variable cells.".into()));
            }
            cons.push(c);
        }
    }
    let non_negative = bool_param(p, "nonNegative").unwrap_or(true);
    let method = match str_param(p, "method").unwrap_or("grg").to_ascii_lowercase().replace([' ', '_', '-'], "").as_str() {
        "grg" | "grgnonlinear" | "nonlinear" => Method::Grg,
        "simplex" | "simplexlp" | "lp" | "linear" => Method::Simplex,
        "evolutionary" => return Err(EngineError::Other("The Evolutionary method isn't available; use GRG Nonlinear or Simplex LP.".into())),
        m => return Err(bad(CMD, format!("unknown method `{m}` (grg or simplex)"))),
    };
    Ok(Model { sheet, objective, goal, vars, vars_text, cons, non_negative, method })
}

// ---------------------------------------------------------------- saved model

fn ref_text(sheet_name: &str, cells: &str) -> String {
    let q = crate::pivot::quote_sheet(sheet_name);
    cells
        .split(',')
        .map(str::trim)
        .filter(|x| !x.is_empty())
        .filter_map(|part| {
            let r = RangeRef::parse(split_sheet(part).1.trim())?;
            let abs = |c: CellRef| format!("${}${}", gridcraft_core::col_to_letters(c.col), c.row as u64 + 1);
            Some(if r.is_single() { format!("{q}!{}", abs(r.start)) } else { format!("{q}!{}:{}", abs(r.start), abs(r.end)) })
        })
        .collect::<Vec<_>>()
        .join(",")
}

/// The model as hidden names scoped to its sheet (replacing any saved before).
fn save_model(wb: &mut Workbook, m: &Model) {
    let Some(sheet_name) = wb.sheet(m.sheet).map(|s| s.name.clone()) else { return };
    wb.names.retain(|n| !(n.scope == Some(m.sheet) && n.name.to_ascii_lowercase().starts_with("solver_")));
    let mut put = |name: String, formula: String| {
        wb.names.push(DefinedName { name, scope: Some(m.sheet), formula, comment: String::new(), hidden: true });
    };
    put("solver_opt".into(), ref_text(&sheet_name, &m.objective.a1()));
    let (typ, val) = match m.goal {
        Goal::Max => (1, 0.0),
        Goal::Min => (2, 0.0),
        Goal::Value(v) => (3, v),
    };
    put("solver_typ".into(), typ.to_string());
    put("solver_val".into(), val.to_string());
    put("solver_adj".into(), ref_text(&sheet_name, &m.vars_text));
    for (i, c) in m.cons.iter().enumerate() {
        let k = i + 1;
        put(format!("solver_lhs{k}"), ref_text(&sheet_name, &c.lhs_text));
        put(format!("solver_rel{k}"), c.rel.code().to_string());
        let rhs = match &c.rhs {
            Rhs::None => {
                if c.rel == Rel::Bin {
                    "\"binary\"".to_string()
                } else {
                    "\"integer\"".to_string()
                }
            }
            Rhs::Const(v) => v.to_string(),
            Rhs::Cells(_) => ref_text(&sheet_name, &c.rhs_text),
        };
        put(format!("solver_rhs{k}"), rhs);
    }
    put("solver_num".into(), m.cons.len().to_string());
    put("solver_neg".into(), if m.non_negative { "1" } else { "2" }.into());
    put("solver_eng".into(), if m.method == Method::Simplex { "2" } else { "1" }.into());
    put("solver_ver".into(), "3".into());
}

/// The saved model of a sheet as `data.solver` params, if any.
pub fn saved_model(wb: &Workbook, sheet: usize) -> Option<Json> {
    let get = |n: &str| {
        wb.names
            .iter()
            .find(|d| d.name.eq_ignore_ascii_case(n) && (d.scope == Some(sheet) || d.scope.is_none()))
            .map(|d| d.formula.trim().trim_start_matches('=').to_string())
    };
    let local = |t: String| -> String { t.split(',').map(|p| split_sheet(p.trim()).1.replace('$', "")).collect::<Vec<_>>().join(",") };
    let objective = local(get("solver_opt")?);
    if objective.is_empty() {
        return None;
    }
    let goal = match get("solver_typ").as_deref().map(str::trim) {
        Some("2") => "min",
        Some("3") => "value",
        _ => "max",
    };
    let value = get("solver_val").and_then(|v| num_text(&v)).unwrap_or(0.0);
    let count = get("solver_num").and_then(|v| num_text(&v)).unwrap_or(0.0).clamp(0.0, MAX_CONSTRAINTS as f64) as usize;
    let mut constraints = Vec::new();
    for k in 1..=count {
        let (Some(lhs), Some(rel)) = (get(&format!("solver_lhs{k}")), get(&format!("solver_rel{k}")).and_then(|r| Rel::from_code(&r))) else {
            continue;
        };
        let lhs = local(lhs);
        constraints.push(match rel {
            Rel::Int | Rel::Bin => json!(format!("{lhs} {}", rel.text())),
            _ => {
                let rhs = get(&format!("solver_rhs{k}")).unwrap_or_default();
                let rhs = if num_text(&rhs).is_some() { rhs } else { local(rhs) };
                json!(format!("{lhs} {} {rhs}", rel.text()))
            }
        });
    }
    Some(json!({
        "objective": objective,
        "goal": goal,
        "value": value,
        "variables": get("solver_adj").map(local).unwrap_or_default(),
        "constraints": constraints,
        "nonNegative": get("solver_neg").as_deref().map(str::trim) != Some("2"),
        "method": if get("solver_eng").as_deref().map(str::trim) == Some("2") { "simplex" } else { "grg" },
    }))
}

// ---------------------------------------------------------------- evaluation

#[derive(Clone, Copy, Debug, PartialEq)]
enum Sense {
    Le,
    Ge,
    Eq,
}

/// A scalar constraint `lhs (sense) rhs`.
#[derive(Clone, Debug)]
struct Row {
    lhs: CellRef,
    sense: Sense,
    rhs: Option<CellRef>,
    rhs_const: f64,
}

/// The values the solvers see at a point: the objective and `lhs − rhs` per row.
#[derive(Clone, Debug)]
struct Point {
    f: f64,
    g: Vec<f64>,
}

struct Evaluator {
    trial: Trial,
    sheet: usize,
    vars: Vec<CellRef>,
    objective: CellRef,
    rows: Vec<Row>,
    evals: u64,
    budget: u64,
}

fn as_num(v: &Value) -> f64 {
    match v {
        Value::Number(n) => *n,
        Value::Empty => 0.0,
        Value::Bool(b) => f64::from(u8::from(*b)),
        _ => f64::NAN,
    }
}

impl Evaluator {
    fn exhausted(&self) -> bool {
        self.evals >= self.budget
    }

    /// Evaluates the model at `x`; `None` once the budget is spent.
    fn eval(&mut self, x: &[f64]) -> Option<Point> {
        if self.exhausted() {
            return None;
        }
        self.evals += 1;
        let sets: Vec<(usize, CellRef, Value)> = self.vars.iter().zip(x).map(|(c, v)| (self.sheet, *c, Value::number(*v))).collect();
        let mut reads = Vec::with_capacity(1 + 2 * self.rows.len());
        reads.push((self.sheet, self.objective));
        for r in &self.rows {
            reads.push((self.sheet, r.lhs));
            if let Some(c) = r.rhs {
                reads.push((self.sheet, c));
            }
        }
        let vals = self.trial.run(&sets, &reads);
        let mut it = vals.iter().map(as_num);
        let f = it.next().unwrap_or(f64::NAN);
        let mut g = Vec::with_capacity(self.rows.len());
        for r in &self.rows {
            let l = it.next().unwrap_or(f64::NAN);
            let rv = if r.rhs.is_some() { it.next().unwrap_or(f64::NAN) } else { r.rhs_const };
            g.push(l - rv);
        }
        Some(Point { f, g })
    }
}

// ---------------------------------------------------------------- problem

struct Problem {
    lo: Vec<f64>,
    hi: Vec<f64>,
    integer: Vec<bool>,
    rows: Vec<Row>,
}

/// Splits the constraints into variable bounds and general rows.
fn build_problem(m: &Model) -> Problem {
    let n = m.vars.len();
    let mut lo = vec![f64::NEG_INFINITY; n];
    let mut hi = vec![f64::INFINITY; n];
    let mut has_lower = vec![false; n];
    let mut integer = vec![false; n];
    let mut rows = Vec::new();
    let var_index = |c: &CellRef| m.vars.iter().position(|v| v == c);
    for c in &m.cons {
        for (k, cell) in c.lhs.iter().enumerate() {
            let j = var_index(cell);
            match c.rel {
                Rel::Int | Rel::Bin => {
                    if let Some(j) = j {
                        if let Some(b) = integer.get_mut(j) {
                            *b = true;
                        }
                        if c.rel == Rel::Bin
                            && let (Some(l), Some(h), Some(hl)) = (lo.get_mut(j), hi.get_mut(j), has_lower.get_mut(j))
                        {
                            *l = l.max(0.0);
                            *h = h.min(1.0);
                            *hl = true;
                        }
                    }
                }
                rel => {
                    let sense = match rel {
                        Rel::Le => Sense::Le,
                        Rel::Ge => Sense::Ge,
                        _ => Sense::Eq,
                    };
                    let (rhs, rhs_const) = match &c.rhs {
                        Rhs::Const(v) => (None, *v),
                        Rhs::Cells(cells) => (cells.get(k).or_else(|| cells.first()).copied(), 0.0),
                        Rhs::None => (None, 0.0),
                    };
                    match (j, rhs) {
                        // A constant bound on a variable cell is a box bound.
                        (Some(j), None) => {
                            if let (Some(l), Some(h), Some(hl)) = (lo.get_mut(j), hi.get_mut(j), has_lower.get_mut(j)) {
                                if sense != Sense::Ge {
                                    *h = h.min(rhs_const);
                                }
                                if sense != Sense::Le {
                                    *l = l.max(rhs_const);
                                    *hl = true;
                                }
                            }
                        }
                        _ => rows.push(Row { lhs: *cell, sense, rhs, rhs_const }),
                    }
                }
            }
        }
    }
    if m.non_negative {
        for (l, hl) in lo.iter_mut().zip(&has_lower) {
            if !hl {
                *l = l.max(0.0);
            }
        }
    }
    Problem { lo, hi, integer, rows }
}

/// What a relaxation (or the whole solve) ended with.
#[derive(Clone, Debug)]
enum Outcome {
    /// x, objective to minimise, and whether the solver converged (vs. ran out of budget).
    Solved(Vec<f64>, f64, bool),
    Infeasible,
    Unbounded,
    Limit,
}

/// Depth-first branch and bound over a relaxation solver; returns the best integer point.
fn branch_and_bound(
    lo: &[f64],
    hi: &[f64],
    integer: &[bool],
    max_nodes: usize,
    gap: f64,
    relax: &mut dyn FnMut(&[f64], &[f64]) -> Outcome,
) -> (Outcome, usize) {
    let mut stack: Vec<(Vec<f64>, Vec<f64>)> = vec![(lo.to_vec(), hi.to_vec())];
    let mut best: Option<(Vec<f64>, f64)> = None;
    let mut nodes = 0usize;
    let mut limited = false;
    let mut root = true;
    while let Some((l, h)) = stack.pop() {
        if nodes >= max_nodes {
            limited = true;
            break;
        }
        nodes += 1;
        let out = relax(&l, &h);
        let was_root = std::mem::replace(&mut root, false);
        let (x, obj, converged) = match out {
            Outcome::Solved(x, obj, c) => (x, obj, c),
            Outcome::Unbounded if was_root => return (Outcome::Unbounded, nodes),
            Outcome::Limit => {
                limited = true;
                break;
            }
            _ => continue,
        };
        if !converged {
            limited = true;
        }
        if let Some((_, b)) = &best
            && obj >= b - gap * b.abs().max(1e-12) - 1e-9 * (1.0 + b.abs())
        {
            continue;
        }
        // The most fractional integer variable.
        let frac = x
            .iter()
            .zip(integer)
            .enumerate()
            .filter(|(_, (_, int))| **int)
            .map(|(j, (v, _))| (j, (v - v.round()).abs()))
            .filter(|(_, f)| *f > 1e-6)
            .max_by(|a, b| a.1.total_cmp(&b.1));
        match frac {
            None => {
                let snapped: Vec<f64> = x.iter().zip(integer).map(|(v, int)| if *int { v.round() } else { *v }).collect();
                best = Some((snapped, obj));
            }
            Some((j, _)) => {
                let v = x.get(j).copied().unwrap_or(0.0);
                let (mut down_h, mut up_l) = (h.clone(), l.clone());
                if let Some(e) = down_h.get_mut(j) {
                    *e = e.min(v.floor());
                }
                if let Some(e) = up_l.get_mut(j) {
                    *e = e.max(v.ceil());
                }
                // Explore the nearer side first (pushed last).
                if v - v.floor() < 0.5 {
                    stack.push((up_l, h));
                    stack.push((l, down_h));
                } else {
                    stack.push((l, down_h));
                    stack.push((up_l, h));
                }
            }
        }
    }
    match best {
        Some((x, obj)) => (Outcome::Solved(x, obj, !limited), nodes),
        None if limited => (Outcome::Limit, nodes),
        None => (Outcome::Infeasible, nodes),
    }
}

// ---------------------------------------------------------------- simplex

/// Pivots the tableau on (r, j), updating the objective row.
fn pivot(t: &mut [Vec<f64>], obj: &mut [f64], r: usize, j: usize) {
    let Some(prow) = t.get_mut(r) else { return };
    let pv = prow.get(j).copied().unwrap_or(0.0);
    if pv == 0.0 {
        return;
    }
    for v in prow.iter_mut() {
        *v /= pv;
    }
    let prow = prow.clone();
    for (i, row) in t.iter_mut().enumerate() {
        if i == r {
            continue;
        }
        let f = row.get(j).copied().unwrap_or(0.0);
        if f != 0.0 {
            for (v, p) in row.iter_mut().zip(&prow) {
                *v -= f * p;
            }
        }
    }
    let f = obj.get(j).copied().unwrap_or(0.0);
    if f != 0.0 {
        for (v, p) in obj.iter_mut().zip(&prow) {
            *v -= f * p;
        }
    }
}

enum Step {
    Optimal,
    Unbounded,
    Limit,
}

/// Minimises with the reduced costs in `obj` (last entry: −objective). Columns not `allowed`
/// never enter.
fn simplex(t: &mut [Vec<f64>], basis: &mut [usize], obj: &mut [f64], allowed: &[bool], budget: &mut u64) -> Step {
    let n = obj.len().saturating_sub(1);
    let cost = (t.len() as u64 + 1).saturating_mul(n as u64 + 1).max(1);
    let mut stalled = 0u32;
    loop {
        if *budget < cost {
            return Step::Limit;
        }
        *budget -= cost;
        let bland = stalled > 50;
        let mut enter = None;
        let mut best = -EPS;
        for (j, d) in obj.iter().take(n).enumerate() {
            if !allowed.get(j).copied().unwrap_or(false) || *d >= -EPS {
                continue;
            }
            if bland {
                enter = Some(j);
                break;
            }
            if *d < best {
                best = *d;
                enter = Some(j);
            }
        }
        let Some(j) = enter else { return Step::Optimal };
        let mut leave: Option<(usize, f64)> = None;
        for (i, row) in t.iter().enumerate() {
            let a = row.get(j).copied().unwrap_or(0.0);
            if a <= EPS {
                continue;
            }
            let ratio = row.get(n).copied().unwrap_or(0.0) / a;
            let better = match leave {
                None => true,
                Some((li, lr)) => ratio < lr - 1e-12 || (ratio <= lr + 1e-12 && basis.get(i) < basis.get(li)),
            };
            if better {
                leave = Some((i, ratio));
            }
        }
        let Some((r, ratio)) = leave else { return Step::Unbounded };
        stalled = if ratio <= EPS { stalled + 1 } else { 0 };
        pivot(t, obj, r, j);
        if let Some(b) = basis.get_mut(r) {
            *b = j;
        }
    }
}

/// Minimises `c·x` subject to `rows` (coefficients, sense, rhs) and `lo ≤ x ≤ hi`.
fn solve_lp(c: &[f64], rows: &[(Vec<f64>, Sense, f64)], lo: &[f64], hi: &[f64], budget: &mut u64) -> Outcome {
    let n = c.len();
    if lo.iter().zip(hi).any(|(l, h)| l > h) {
        return Outcome::Infeasible;
    }
    // x_j = offset + Σ sign·y_k with y ≥ 0.
    struct Map {
        var: usize,
        sign: f64,
    }
    let mut cols: Vec<Map> = Vec::new();
    let mut offset = vec![0.0; n];
    let mut extra: Vec<(Vec<(usize, f64)>, Sense, f64)> = Vec::new();
    for j in 0..n {
        let (l, h) = (lo.get(j).copied().unwrap_or(f64::NEG_INFINITY), hi.get(j).copied().unwrap_or(f64::INFINITY));
        if l.is_finite() {
            if let Some(o) = offset.get_mut(j) {
                *o = l;
            }
            cols.push(Map { var: j, sign: 1.0 });
            if h.is_finite() {
                extra.push((vec![(cols.len() - 1, 1.0)], Sense::Le, h - l));
            }
        } else if h.is_finite() {
            if let Some(o) = offset.get_mut(j) {
                *o = h;
            }
            cols.push(Map { var: j, sign: -1.0 });
        } else {
            cols.push(Map { var: j, sign: 1.0 });
            cols.push(Map { var: j, sign: -1.0 });
        }
    }
    let ny = cols.len();
    // Rows over y: (coefficients, sense, rhs).
    let mut yrows: Vec<(Vec<f64>, Sense, f64)> = Vec::with_capacity(rows.len() + extra.len());
    for (a, sense, b) in rows {
        let mut coef = vec![0.0; ny];
        for (k, m) in cols.iter().enumerate() {
            if let Some(v) = coef.get_mut(k) {
                *v = a.get(m.var).copied().unwrap_or(0.0) * m.sign;
            }
        }
        let shift: f64 = a.iter().zip(&offset).map(|(x, o)| x * o).sum();
        yrows.push((coef, *sense, b - shift));
    }
    for (entries, sense, b) in extra {
        let mut coef = vec![0.0; ny];
        for (k, v) in entries {
            if let Some(e) = coef.get_mut(k) {
                *e = v;
            }
        }
        yrows.push((coef, sense, b));
    }
    let m = yrows.len();
    let n_slack = yrows.iter().filter(|r| r.1 != Sense::Eq).count();
    // Rows with b < 0 are negated so every right side is ≥ 0.
    let normalized: Vec<(Vec<f64>, Sense, f64)> = yrows
        .into_iter()
        .map(|(a, s, b)| {
            if b < 0.0 {
                let s = match s {
                    Sense::Le => Sense::Ge,
                    Sense::Ge => Sense::Le,
                    Sense::Eq => Sense::Eq,
                };
                (a.iter().map(|v| -v).collect(), s, -b)
            } else {
                (a, s, b)
            }
        })
        .collect();
    let n_art = normalized.iter().filter(|r| r.1 != Sense::Le).count();
    let width = ny + n_slack + n_art;
    let mut t: Vec<Vec<f64>> = Vec::with_capacity(m);
    let mut basis = vec![0usize; m];
    let (mut si, mut ai) = (ny, ny + n_slack);
    for (i, (a, s, b)) in normalized.iter().enumerate() {
        let mut row = vec![0.0; width + 1];
        for (v, x) in row.iter_mut().zip(a) {
            *v = *x;
        }
        match s {
            Sense::Le => {
                if let Some(v) = row.get_mut(si) {
                    *v = 1.0;
                }
                if let Some(bi) = basis.get_mut(i) {
                    *bi = si;
                }
                si += 1;
            }
            Sense::Ge => {
                if let Some(v) = row.get_mut(si) {
                    *v = -1.0;
                }
                si += 1;
                if let Some(v) = row.get_mut(ai) {
                    *v = 1.0;
                }
                if let Some(bi) = basis.get_mut(i) {
                    *bi = ai;
                }
                ai += 1;
            }
            Sense::Eq => {
                if let Some(v) = row.get_mut(ai) {
                    *v = 1.0;
                }
                if let Some(bi) = basis.get_mut(i) {
                    *bi = ai;
                }
                ai += 1;
            }
        }
        if let Some(v) = row.get_mut(width) {
            *v = *b;
        }
        t.push(row);
    }
    let is_art = |j: usize| j >= ny + n_slack && j < width;
    // Phase 1: minimise the sum of the artificials.
    let mut obj = vec![0.0; width + 1];
    for j in (ny + n_slack)..width {
        if let Some(v) = obj.get_mut(j) {
            *v = 1.0;
        }
    }
    for (i, row) in t.iter().enumerate() {
        if basis.get(i).is_some_and(|b| is_art(*b)) {
            for (v, x) in obj.iter_mut().zip(row) {
                *v -= x;
            }
        }
    }
    let all: Vec<bool> = vec![true; width];
    if n_art > 0 {
        match simplex(&mut t, &mut basis, &mut obj, &all, budget) {
            Step::Limit => return Outcome::Limit,
            Step::Unbounded | Step::Optimal => {}
        }
        let scale = normalized.iter().map(|r| r.2.abs()).fold(1.0f64, f64::max);
        if -obj.get(width).copied().unwrap_or(0.0) > 1e-7 * scale {
            return Outcome::Infeasible;
        }
        // Drive zero-valued artificials out of the basis where possible.
        for i in 0..m {
            if !basis.get(i).is_some_and(|b| is_art(*b)) {
                continue;
            }
            let j = t.get(i).and_then(|row| (0..ny + n_slack).find(|&j| row.get(j).is_some_and(|v| v.abs() > EPS)));
            if let Some(j) = j {
                pivot(&mut t, &mut obj, i, j);
                if let Some(b) = basis.get_mut(i) {
                    *b = j;
                }
            }
        }
    }
    // Phase 2: the real costs; artificials may not re-enter.
    let mut cost = vec![0.0; width + 1];
    for (k, mp) in cols.iter().enumerate() {
        if let Some(v) = cost.get_mut(k) {
            *v = c.get(mp.var).copied().unwrap_or(0.0) * mp.sign;
        }
    }
    let mut obj = cost.clone();
    for (i, row) in t.iter().enumerate() {
        let cb = basis.get(i).and_then(|b| cost.get(*b)).copied().unwrap_or(0.0);
        if cb != 0.0 {
            for (v, x) in obj.iter_mut().zip(row) {
                *v -= cb * x;
            }
        }
    }
    let allowed: Vec<bool> = (0..width).map(|j| !is_art(j)).collect();
    match simplex(&mut t, &mut basis, &mut obj, &allowed, budget) {
        Step::Limit => return Outcome::Limit,
        Step::Unbounded => return Outcome::Unbounded,
        Step::Optimal => {}
    }
    let mut y = vec![0.0; width];
    for (i, row) in t.iter().enumerate() {
        if let (Some(b), Some(v)) = (basis.get(i), row.get(width))
            && let Some(e) = y.get_mut(*b)
        {
            *e = *v;
        }
    }
    let mut x = offset;
    for (k, mp) in cols.iter().enumerate() {
        if let Some(e) = x.get_mut(mp.var) {
            *e += mp.sign * y.get(k).copied().unwrap_or(0.0);
        }
    }
    let val: f64 = c.iter().zip(&x).map(|(a, b)| a * b).sum();
    Outcome::Solved(x, val, true)
}

/// Linearises the model at `x0` (n + 1 evaluations) and checks it at one more point. Returns the
/// objective gradient and constant, and per row the gradient and constant of `lhs − rhs`.
#[allow(clippy::type_complexity)]
fn linearize(ev: &mut Evaluator, x0: &[f64]) -> std::result::Result<((Vec<f64>, f64), Vec<(Vec<f64>, f64)>), &'static str> {
    let n = x0.len();
    let p0 = ev.eval(x0).ok_or("budget")?;
    if !p0.f.is_finite() || p0.g.iter().any(|v| !v.is_finite()) {
        return Err("error");
    }
    let mut cf = vec![0.0; n];
    let mut ga = vec![vec![0.0; n]; p0.g.len()];
    for j in 0..n {
        let mut x = x0.to_vec();
        if let Some(v) = x.get_mut(j) {
            *v += 1.0;
        }
        let p = ev.eval(&x).ok_or("budget")?;
        if let Some(v) = cf.get_mut(j) {
            *v = p.f - p0.f;
        }
        for (row, (gi, g0)) in ga.iter_mut().zip(p.g.iter().zip(&p0.g)) {
            if let Some(v) = row.get_mut(j) {
                *v = gi - g0;
            }
        }
    }
    let dot = |a: &[f64], x: &[f64]| a.iter().zip(x).map(|(p, q)| p * q).sum::<f64>();
    let f_const = p0.f - dot(&cf, x0);
    let rows: Vec<(Vec<f64>, f64)> = ga.into_iter().zip(&p0.g).map(|(a, g0)| (a.clone(), g0 - dot(&a, x0))).collect();
    // Linearity check at a spread-out point.
    let xt: Vec<f64> =
        x0.iter().enumerate().map(|(j, v)| v + if j % 2 == 0 { 1.7 + 0.3 * (j % 5) as f64 } else { -0.6 - 0.2 * (j % 3) as f64 }).collect();
    let pt = ev.eval(&xt).ok_or("budget")?;
    let close = |actual: f64, pred: f64, terms: f64| actual.is_finite() && (actual - pred).abs() <= 1e-6 * (1.0 + actual.abs() + terms);
    let terms = |a: &[f64]| a.iter().zip(&xt).map(|(p, q)| (p * q).abs()).sum::<f64>();
    if !close(pt.f, f_const + dot(&cf, &xt), terms(&cf)) {
        return Err("nonlinear");
    }
    for ((a, k), g) in rows.iter().zip(&pt.g) {
        if !close(*g, k + dot(a, &xt), terms(a)) {
            return Err("nonlinear");
        }
    }
    Ok(((cf, f_const), rows))
}

// ---------------------------------------------------------------- nonlinear

/// Augmented-Lagrangian value of a point: objective `phi` plus the constraint terms.
fn aug(phi: f64, cons: &[f64], senses: &[Sense], lam: &[f64], mu: f64) -> f64 {
    let mut v = phi;
    for ((c, s), l) in cons.iter().zip(senses).zip(lam) {
        v += match s {
            Sense::Eq => l * c + 0.5 * mu * c * c,
            _ => {
                let t = (l + mu * c).max(0.0);
                (t * t - l * l) / (2.0 * mu)
            }
        };
    }
    v
}

/// Constraint values in the `≤ 0` / `= 0` form, and the objective to minimise.
fn shape(p: &Point, rows: &[Row], goal: Goal) -> (f64, Vec<f64>) {
    let phi = match goal {
        Goal::Max => -p.f,
        Goal::Min => p.f,
        Goal::Value(v) => (p.f - v) * (p.f - v),
    };
    let cons = p.g.iter().zip(rows).map(|(g, r)| if r.sense == Sense::Ge { -g } else { *g }).collect();
    (phi, cons)
}

fn violation(cons: &[f64], senses: &[Sense]) -> f64 {
    cons.iter().zip(senses).map(|(c, s)| if *s == Sense::Eq { c.abs() } else { c.max(0.0) }).fold(0.0, f64::max)
}

fn project(x: &mut [f64], lo: &[f64], hi: &[f64]) {
    for ((v, l), h) in x.iter_mut().zip(lo).zip(hi) {
        if *v < *l {
            *v = *l;
        }
        if *v > *h {
            *v = *h;
        }
    }
}

/// Local minimisation of the model within bounds `lo..hi` from `x0`.
fn nlp(ev: &mut Evaluator, x0: &[f64], lo: &[f64], hi: &[f64], goal: Goal) -> Outcome {
    let n = x0.len();
    let rows = ev.rows.clone();
    let senses: Vec<Sense> = rows.iter().map(|r| r.sense).collect();
    let mut x = x0.to_vec();
    project(&mut x, lo, hi);
    let mut lam = vec![0.0; rows.len()];
    let mut mu = 10.0;
    let Some(p) = ev.eval(&x) else { return Outcome::Limit };
    let (mut phi, mut cons) = shape(&p, &rows, goal);
    if !phi.is_finite() {
        return Outcome::Infeasible;
    }
    let phi0 = phi;
    let mut prev_viol = violation(&cons, &senses);
    let mut converged = false;
    let mut out_of_budget = false;
    let mut carried: Option<f64> = None;
    for _outer in 0..40 {
        // Inner: projected gradient on the augmented Lagrangian.
        let lval = |phi: f64, cons: &[f64], lam: &[f64], mu: f64| aug(phi, cons, &senses, lam, mu);
        let grad = |ev: &mut Evaluator, x: &[f64], lam: &[f64], mu: f64| -> Option<Vec<f64>> {
            let mut gr = vec![0.0; n];
            for j in 0..n {
                let xj = x.get(j).copied().unwrap_or(0.0);
                let h = 1e-6 * xj.abs().max(1.0);
                let (l, u) = (lo.get(j).copied().unwrap_or(f64::NEG_INFINITY), hi.get(j).copied().unwrap_or(f64::INFINITY));
                let (a, b) = (if xj - h >= l { xj - h } else { xj }, if xj + h <= u { xj + h } else { xj });
                if b <= a {
                    continue;
                }
                let mut xa = x.to_vec();
                let mut xb = x.to_vec();
                if let Some(v) = xa.get_mut(j) {
                    *v = a;
                }
                if let Some(v) = xb.get_mut(j) {
                    *v = b;
                }
                let (pa, ca) = shape(&ev.eval(&xa)?, &rows, goal);
                let (pb, cb) = shape(&ev.eval(&xb)?, &rows, goal);
                let d = (lval(pb, &cb, lam, mu) - lval(pa, &ca, lam, mu)) / (b - a);
                if let Some(v) = gr.get_mut(j) {
                    *v = if d.is_finite() { d } else { 0.0 };
                }
            }
            Some(gr)
        };
        let mut l_cur = lval(phi, &cons, &lam, mu);
        let Some(mut g) = grad(ev, &x, &lam, mu) else {
            out_of_budget = true;
            break;
        };
        // The step length carries over between outer iterations once known.
        let gnorm = g.iter().map(|v| v.abs()).fold(0.0, f64::max);
        let mut alpha = carried.unwrap_or(if gnorm > 0.0 { 1.0 / gnorm } else { 1.0 });
        let mut inner_done = false;
        let mut quiet = 0;
        for _inner in 0..500 {
            let mut accepted = None;
            let mut a = alpha;
            for _ in 0..40 {
                let mut xn: Vec<f64> = x.iter().zip(&g).map(|(v, d)| v - a * d).collect();
                project(&mut xn, lo, hi);
                let Some(pn) = ev.eval(&xn) else { break };
                let (phn, cn) = shape(&pn, &rows, goal);
                let ln = lval(phn, &cn, &lam, mu);
                let decrease: f64 = g.iter().zip(x.iter().zip(&xn)).map(|(d, (u, v))| d * (u - v)).sum();
                if ln.is_finite() && ln <= l_cur - 1e-4 * decrease {
                    accepted = Some((xn, phn, cn, ln));
                    break;
                }
                a *= 0.25;
            }
            if ev.exhausted() {
                out_of_budget = true;
            }
            let Some((xn, phn, cn, ln)) = accepted else {
                inner_done = true;
                break;
            };
            // Variables or objective running off towards infinity: unbounded.
            if phn.abs() > 1e15 * (1.0 + phi0.abs()) || xn.iter().zip(x0).any(|(v, s)| v.abs() > 1e15 * (1.0 + s.abs())) {
                return Outcome::Unbounded;
            }
            let s: Vec<f64> = xn.iter().zip(&x).map(|(a, b)| a - b).collect();
            let step = s.iter().map(|v| v.abs()).fold(0.0, f64::max);
            let xmax = xn.iter().map(|v| v.abs()).fold(1.0, f64::max);
            let small = step <= 1e-10 * xmax || (l_cur - ln).abs() <= 1e-13 * (1.0 + l_cur.abs());
            quiet = if small { quiet + 1 } else { 0 };
            x = xn;
            phi = phn;
            cons = cn;
            l_cur = ln;
            if quiet >= 3 {
                inner_done = true;
                break;
            }
            let Some(gn) = grad(ev, &x, &lam, mu) else {
                out_of_budget = true;
                break;
            };
            // Barzilai–Borwein step.
            let yv: Vec<f64> = gn.iter().zip(&g).map(|(a, b)| a - b).collect();
            let sy: f64 = s.iter().zip(&yv).map(|(a, b)| a * b).sum();
            let ss: f64 = s.iter().map(|v| v * v).sum();
            alpha = if sy > 0.0 { (ss / sy).clamp(1e-12, 1e12) } else { (a * 4.0).min(1e12) };
            g = gn;
            if g.iter().all(|v| *v == 0.0) {
                inner_done = true;
                break;
            }
        }
        carried = Some(alpha);
        if out_of_budget {
            break;
        }
        let viol = violation(&cons, &senses);
        let scale = rows.iter().map(|r| r.rhs_const.abs()).fold(1.0, f64::max);
        if viol <= PRECISION * scale && inner_done {
            converged = true;
            if rows.is_empty() {
                break;
            }
        }
        // Multiplier and penalty updates.
        for ((l, c), s) in lam.iter_mut().zip(&cons).zip(&senses) {
            *l = match s {
                Sense::Eq => *l + mu * c,
                _ => (*l + mu * c).max(0.0),
            };
        }
        if viol > 0.25 * prev_viol {
            mu = (mu * 10.0).min(1e10);
        }
        if converged && viol <= PRECISION * scale * 1e-2 {
            break;
        }
        prev_viol = viol;
        converged = false;
    }
    let viol = violation(&cons, &senses);
    let scale = rows.iter().map(|r| r.rhs_const.abs()).fold(1.0, f64::max);
    if viol > PRECISION * scale * 10.0 {
        return if out_of_budget { Outcome::Limit } else { Outcome::Infeasible };
    }
    Outcome::Solved(x, phi, (converged || viol <= PRECISION * scale) && !out_of_budget)
}

// ---------------------------------------------------------------- command

fn run(s: &mut Session, p: &Json) -> Result<Json> {
    let d = s.doc()?;
    let active = d.wb.active_sheet;
    let mut params = p.clone();
    if bool_param(p, "saved").unwrap_or(false) {
        let saved = saved_model(&d.wb, active).ok_or_else(|| EngineError::Other("This sheet has no saved Solver model.".into()))?;
        if let (Some(base), Some(over)) = (saved.as_object(), p.as_object()) {
            let mut merged = base.clone();
            for (k, v) in over {
                merged.insert(k.clone(), v.clone());
            }
            params = Json::Object(merged);
        }
    }
    let has_objective = ["objective", "setObjective", "set"].iter().any(|k| str_param(&params, k).is_some_and(|t| !t.trim().is_empty()));
    if !has_objective {
        let model = saved_model(&d.wb, active);
        s.ui_requests.push(crate::UiRequest::Dialog("solver".into(), json!({})));
        return Ok(json!({"model": model}));
    }
    let model = parse_model(s, &params)?;
    let budget = params
        .get("maxEvaluations")
        .and_then(Json::as_f64)
        .filter(|v| v.is_finite() && *v >= 1.0)
        .map_or(DEFAULT_EVALS, |v| (v as u64).min(MAX_EVALS));
    let gap = params.get("integerOptimality").and_then(Json::as_f64).filter(|v| v.is_finite()).map_or(0.0, |v| v.clamp(0.0, 100.0) / 100.0);
    let keep = bool_param(&params, "keep").unwrap_or(true);
    let save = bool_param(&params, "saveModel").unwrap_or(true);
    let prob = build_problem(&model);
    let d = s.doc()?;
    let sh = d.wb.sheet(model.sheet).ok_or(EngineError::NoDocument)?;
    let x0: Vec<f64> = model.vars.iter().map(|c| as_num(&sh.value(*c))).map(|v| if v.is_finite() { v } else { 0.0 }).collect();
    let mut ev = Evaluator {
        trial: Trial::new(d),
        sheet: model.sheet,
        vars: model.vars.clone(),
        objective: model.objective,
        rows: prob.rows.clone(),
        evals: 0,
        budget,
    };
    let any_int = prob.integer.iter().any(|b| *b);
    let mut not_linear = false;
    let outcome = if prob.lo.iter().zip(&prob.hi).any(|(l, h)| l > h) {
        Outcome::Infeasible
    } else {
        match model.method {
            Method::Simplex => match linearize(&mut ev, &x0) {
                Err("nonlinear") | Err("error") => {
                    not_linear = true;
                    Outcome::Infeasible
                }
                Err(_) => Outcome::Limit,
                Ok(((cf, f_const), lin)) => {
                    let mut rows: Vec<(Vec<f64>, Sense, f64)> = lin.iter().zip(&prob.rows).map(|((a, k), r)| (a.clone(), r.sense, -k)).collect();
                    let c: Vec<f64> = match model.goal {
                        Goal::Max => cf.iter().map(|v| -v).collect(),
                        Goal::Min => cf.clone(),
                        Goal::Value(v) => {
                            rows.push((cf.clone(), Sense::Eq, v - f_const));
                            vec![0.0; cf.len()]
                        }
                    };
                    let size = (rows.len() as u64 + 2 * model.vars.len() as u64 + 1) * (4 * model.vars.len() as u64 + 2 * rows.len() as u64 + 1);
                    let mut work = LP_WORK.max(size);
                    let mut relax = |l: &[f64], h: &[f64]| solve_lp(&c, &rows, l, h, &mut work);
                    if any_int {
                        branch_and_bound(&prob.lo, &prob.hi, &prob.integer, MAX_NODES_LP, gap, &mut relax).0
                    } else {
                        relax(&prob.lo, &prob.hi)
                    }
                }
            },
            Method::Grg => {
                let goal = model.goal;
                if any_int {
                    let mut relax = |l: &[f64], h: &[f64]| {
                        let mut start = x0.clone();
                        project(&mut start, l, h);
                        nlp(&mut ev, &start, l, h, goal)
                    };
                    branch_and_bound(&prob.lo, &prob.hi, &prob.integer, MAX_NODES_NLP, gap, &mut relax).0
                } else {
                    nlp(&mut ev, &x0, &prob.lo, &prob.hi, goal)
                }
            }
        }
    };
    let (status, message, solution) = match outcome {
        _ if not_linear => ("notLinear", "The model isn't linear in the variable cells, so Simplex LP can't solve it. Try GRG Nonlinear.", None),
        Outcome::Solved(x, _, true) => (
            if model.method == Method::Simplex { "optimal" } else { "converged" },
            if model.method == Method::Simplex {
                "Solver found an optimal solution. All constraints are satisfied."
            } else {
                "Solver converged to a locally optimal solution. All constraints are satisfied."
            },
            Some(x),
        ),
        Outcome::Solved(x, _, false) => {
            ("iterationLimit", "Solver stopped at its evaluation limit; the best solution found satisfies the constraints.", Some(x))
        }
        Outcome::Limit => ("iterationLimit", "Solver stopped at its evaluation limit without finding a feasible solution.", None),
        Outcome::Infeasible => ("infeasible", "Solver could not find a solution that satisfies all constraints.", None),
        Outcome::Unbounded => ("unbounded", "The objective can grow without limit: the problem is unbounded.", None),
    };
    let evaluations = ev.evals;
    // The objective's value at the solution.
    let objective = solution.as_ref().and_then(|x| {
        ev.budget = ev.budget.saturating_add(1);
        ev.eval(x).map(|p| p.f)
    });
    let mut values = serde_json::Map::new();
    if let Some(x) = &solution {
        for (c, v) in model.vars.iter().zip(x) {
            values.insert(c.a1(), json!(v));
        }
    }
    let changed = keep && solution.is_some();
    if changed || save {
        let sheet = model.sheet;
        let vars = model.vars.clone();
        let m = model.clone();
        edit(s, |cx| {
            if changed && let Some(x) = &solution {
                let sh = cx.sheet_mut(sheet)?;
                for (c, v) in vars.iter().zip(x) {
                    let style = sh.style_id(*c);
                    sh.set_cell(*c, Cell { value: Value::number(*v), formula: None, style });
                }
                for c in &vars {
                    cx.touch(sheet, *c);
                }
            }
            if save {
                save_model(&mut cx.wb, &m);
            }
            Ok(())
        })?;
    }
    Ok(json!({
        "status": status,
        "message": message,
        "objective": objective,
        "values": values,
        "evaluations": evaluations,
        "changed": changed,
    }))
}

#[cfg(test)]
mod tests {
    use gridcraft_core::CellRef;
    use serde_json::json;

    use crate::Session;

    fn s() -> Session {
        let mut s = Session::new();
        s.new_workbook();
        s
    }
    fn n(s: &Session, a: &str) -> f64 {
        let d = s.doc().unwrap();
        d.wb.active().unwrap().value(CellRef::parse(a).unwrap()).as_f64().unwrap_or(f64::NAN)
    }
    fn set(s: &mut Session, cells: &[(&str, &str)]) {
        for (c, v) in cells {
            s.execute("cell.set", json!({"cell": c, "input": v})).unwrap();
        }
    }

    #[test]
    fn simplex_solves_the_wyndor_glass_lp() {
        // Hillier & Lieberman's Wyndor Glass problem: max 3x + 5y, x ≤ 4, 2y ≤ 12, 3x + 2y ≤ 18
        // → x = 2, y = 6, Z = 36.
        let mut s = s();
        set(&mut s, &[("A1", "0"), ("A2", "0"), ("B1", "=3*A1+5*A2"), ("B2", "=2*A2"), ("B3", "=3*A1+2*A2")]);
        let r = s
            .execute(
                "data.solver",
                json!({"objective": "B1", "goal": "max", "variables": "A1:A2", "constraints": ["A1 <= 4", "B2 <= 12", {"lhs": "B3", "op": "<=", "rhs": 18}], "method": "simplex"}),
            )
            .unwrap();
        assert_eq!(r["status"], "optimal", "{r}");
        assert!((n(&s, "A1") - 2.0).abs() < 1e-9 && (n(&s, "A2") - 6.0).abs() < 1e-9);
        assert!((n(&s, "B1") - 36.0).abs() < 1e-9);
        // The model is saved as hidden names, survives XLSX and solves again from them.
        let names: Vec<String> = s.doc().unwrap().wb.names.iter().map(|n| n.name.clone()).collect();
        assert!(names.iter().any(|n| n == "solver_opt") && names.iter().any(|n| n == "solver_lhs3"));
        let b64 = s.execute("file.saveBytes", json!({"format": "xlsx"})).unwrap()["base64"].as_str().unwrap().to_string();
        s.execute("file.open", json!({"name": "m.xlsx", "base64": b64})).unwrap();
        set(&mut s, &[("A1", "0"), ("A2", "0")]);
        let m = s.execute("data.solver", json!({})).unwrap();
        assert_eq!(m["model"]["objective"], "B1");
        assert_eq!(m["model"]["method"], "simplex");
        let r = s.execute("data.solver", json!({"saved": true})).unwrap();
        assert_eq!(r["status"], "optimal");
        assert!((n(&s, "B1") - 36.0).abs() < 1e-9);
        // Minimise with keep: false leaves the cells alone.
        set(&mut s, &[("A1", "1"), ("A2", "1")]);
        let r = s.execute("data.solver", json!({"saved": true, "goal": "min", "keep": false})).unwrap();
        assert_eq!(r["objective"], 0.0);
        assert_eq!((n(&s, "A1"), n(&s, "A2")), (1.0, 1.0));
    }

    #[test]
    fn integer_and_binary_constraints_use_branch_and_bound() {
        // max 5x + 4y, 6x + 4y ≤ 24, x + 2y ≤ 6: the LP optimum (3, 1.5) = 21; integers give
        // (4, 0) = 20.
        let mut s = s();
        set(&mut s, &[("B1", "=5*A1+4*A2"), ("B2", "=6*A1+4*A2"), ("B3", "=A1+2*A2")]);
        let base = json!({"objective": "B1", "variables": "A1:A2", "constraints": ["B2 <= 24", "B3 <= 6"], "method": "simplex"});
        let r = s.execute("data.solver", base.clone()).unwrap();
        assert!((r["objective"].as_f64().unwrap() - 21.0).abs() < 1e-9);
        let mut p = base.clone();
        p["constraints"].as_array_mut().unwrap().push(json!("A1:A2 int"));
        let r = s.execute("data.solver", p).unwrap();
        assert_eq!(r["status"], "optimal");
        assert_eq!((n(&s, "A1"), n(&s, "A2"), n(&s, "B1")), (4.0, 0.0, 20.0));
        // 0/1 knapsack: values 10, 13, 7, weights 5, 6, 4, capacity 10 → items 2 and 3 (20).
        set(&mut s, &[("D1", "=10*C1+13*C2+7*C3"), ("D2", "=5*C1+6*C2+4*C3")]);
        let r = s
            .execute("data.solver", json!({"objective": "D1", "variables": "C1:C3", "constraints": ["D2 <= 10", "C1:C3 bin"], "method": "simplex"}))
            .unwrap();
        assert_eq!(r["status"], "optimal");
        assert_eq!((n(&s, "C1"), n(&s, "C2"), n(&s, "C3"), n(&s, "D1")), (0.0, 1.0, 1.0, 20.0));
    }

    #[test]
    fn grg_finds_smooth_optima() {
        // Largest rectangle with perimeter at most 20: a 5 × 5 square, area 25.
        let mut s = s();
        set(&mut s, &[("A1", "1"), ("A2", "1"), ("B1", "=A1*A2"), ("B2", "=2*A1+2*A2")]);
        let r = s.execute("data.solver", json!({"objective": "B1", "variables": "A1:A2", "constraints": ["B2 <= 20"]})).unwrap();
        assert_eq!(r["status"], "converged", "{r}");
        assert!((n(&s, "A1") - 5.0).abs() < 1e-3 && (n(&s, "A2") - 5.0).abs() < 1e-3, "{r}");
        // Unconstrained minimum of (x − 3)² + (y + 1)² at (3, −1) once negatives are allowed.
        set(&mut s, &[("C1", "0"), ("C2", "0"), ("D1", "=(C1-3)^2+(C2+1)^2")]);
        s.execute("data.solver", json!({"objective": "D1", "goal": "min", "variables": "C1:C2", "nonNegative": false})).unwrap();
        assert!((n(&s, "C1") - 3.0).abs() < 1e-4 && (n(&s, "C2") + 1.0).abs() < 1e-4);
        // Value Of: x³ = 8 → x = 2.
        set(&mut s, &[("E1", "1"), ("F1", "=E1^3")]);
        s.execute("data.solver", json!({"objective": "F1", "goal": "value", "value": 8, "variables": "E1"})).unwrap();
        assert!((n(&s, "E1") - 2.0).abs() < 1e-4);
    }

    #[test]
    fn infeasible_unbounded_and_nonlinear_models_are_reported() {
        let mut s = s();
        set(&mut s, &[("A1", "1"), ("B1", "=2*A1")]);
        for method in ["simplex", "grg"] {
            let r = s
                .execute("data.solver", json!({"objective": "B1", "variables": "A1", "constraints": ["A1 >= 5", "B1 <= 6"], "method": method}))
                .unwrap();
            assert_eq!(r["status"], "infeasible", "{method}: {r}");
            assert_eq!(r["changed"], false);
            assert_eq!(n(&s, "A1"), 1.0, "an infeasible model leaves the cells alone");
        }
        let r = s.execute("data.solver", json!({"objective": "B1", "variables": "A1", "method": "simplex"})).unwrap();
        assert_eq!(r["status"], "unbounded");
        let r = s.execute("data.solver", json!({"objective": "B1", "variables": "A1", "method": "grg", "maxEvaluations": 5000})).unwrap();
        assert!(r["status"] == "unbounded" || r["status"] == "iterationLimit", "{r}");
        set(&mut s, &[("A1", "1"), ("C1", "=A1^2")]);
        let r = s.execute("data.solver", json!({"objective": "C1", "goal": "min", "variables": "A1", "method": "simplex"})).unwrap();
        assert_eq!(r["status"], "notLinear");
    }

    #[test]
    fn hostile_models_are_refused_without_panicking() {
        let mut s = s();
        let r = s.execute("data.solver", json!({})).unwrap();
        assert!(r["model"].is_null());
        assert!(s.take_ui_requests().iter().any(|q| matches!(q, crate::UiRequest::Dialog(n, _) if n == "solver")));
        set(&mut s, &[("B1", "=SUM(A1:A1000)"), ("C1", "5")]);
        for p in [
            json!({"objective": "B1", "variables": "A1:A1000"}),
            json!({"objective": "B1", "variables": "A1:XFD1048576"}),
            json!({"objective": "C1", "variables": "A1"}),
            json!({"objective": "B1:B2", "variables": "A1"}),
            json!({"objective": "B1", "variables": "B1"}),
            json!({"objective": "B1", "variables": "A1", "goal": "sideways"}),
            json!({"objective": "B1", "variables": "A1", "goal": "value"}),
            json!({"objective": "B1", "variables": "A1", "method": "evolutionary"}),
            json!({"objective": "B1", "variables": "A1", "constraints": ["A1 ?? 3"]}),
            json!({"objective": "B1", "variables": "A1", "constraints": "A1 <= 3"}),
            json!({"objective": "B1", "variables": "A1", "constraints": ["A1:A3 <= C1:C2"]}),
            json!({"objective": "B1", "variables": "A1", "constraints": ["C1 int"]}),
            json!({"objective": "B1", "variables": "A1", "constraints": ["A1:A1048576 <= 1"]}),
            json!({"objective": "B1", "variables": "A1", "constraints": vec![json!("A1 <= 1"); 1001]}),
            json!({"objective": "Nope!B1", "variables": "A1"}),
        ] {
            assert!(s.execute("data.solver", p.clone()).is_err(), "{p}");
        }
        // Huge evaluation requests are capped, and a model erroring everywhere doesn't panic.
        set(&mut s, &[("D1", "=1/0+A1")]);
        let r = s.execute("data.solver", json!({"objective": "D1", "variables": "A1", "maxEvaluations": 1e300})).unwrap();
        assert_ne!(r["status"], "converged");
    }
}
