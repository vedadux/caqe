extern crate cryptominisat;
use self::cryptominisat::*;

extern crate bit_vec;
use self::bit_vec::BitVec;

use super::*;

use std::collections::HashMap;

use std::fmt;

#[cfg(feature = "statistics")]
use super::utils::statistics::TimingStats;

type QMatrix = Matrix<TreePrefix>;

pub struct CaqeSolver<'a> {
    matrix: &'a QMatrix,
    result: SolverResult,
    abstraction: Vec<Box<ScopeRecursiveSolver>>,
}

impl<'a> CaqeSolver<'a> {
    pub fn new(matrix: &QMatrix) -> CaqeSolver {
        Self::new_with_options(matrix, CaqeSolverOptions::new())
    }

    pub fn new_with_options(matrix: &QMatrix, options: CaqeSolverOptions) -> CaqeSolver {
        let mut abstractions = Vec::new();
        for scope_node in matrix.prefix.roots.iter() {
            abstractions.push(ScopeRecursiveSolver::init_abstraction_recursively(
                matrix, options, scope_node,
            ));
        }
        debug_assert!(!matrix.conflict());
        CaqeSolver {
            matrix: matrix,
            result: SolverResult::Unknown,
            abstraction: abstractions,
        }
    }

    #[cfg(feature = "statistics")]
    pub fn print_statistics(&self) {
        for ref abstraction in self.abstraction.iter() {
            abstraction.print_statistics();
        }
    }

    pub fn qdimacs_output(&self) -> qdimacs::PartialQDIMACSCertificate {
        let mut certificate = qdimacs::PartialQDIMACSCertificate::new(
            self.result,
            self.matrix.prefix.variables().orig_num_variables(),
            self.matrix.orig_clause_num,
        );

        if self.result == SolverResult::Unknown {
            return certificate;
        }

        // get the first scope that contains variables (the scope 0 may be empty)
        let mut top_level = Vec::new();
        let is_universal;
        if self.matrix
            .prefix
            .roots
            .iter()
            .fold(true, |val, node| val && node.scope.variables.is_empty())
        {
            // top-level existential scope is empty
            for abstraction in self.abstraction.iter() {
                top_level.extend(&abstraction.next);
            }
            is_universal = true;
        } else {
            top_level.extend(&self.abstraction);
            is_universal = false;
        }

        // output the variable assignment if possible
        if self.result == SolverResult::Satisfiable && is_universal
            || self.result == SolverResult::Unsatisfiable && !is_universal
        {
            return certificate;
        }

        // go thorough all scopes in the level
        // for existential level: combine the assignments
        // for universal level: select only one level
        for scope in top_level.iter() {
            if self.result == SolverResult::Unsatisfiable
                && scope.data.sub_result != SolverResult::Unsatisfiable
            {
                continue;
            }

            for variable in scope.data.variables.iter() {
                let value = scope.data.assignments[variable];
                let info = &self.matrix.prefix.variables().get(*variable);
                let mut orig_variable;
                if info.copy_of != 0 {
                    orig_variable = info.copy_of;
                } else {
                    orig_variable = *variable;
                }
                certificate.add_assignment(Literal::new(orig_variable, !value));
            }

            if self.result == SolverResult::Unsatisfiable {
                // only one assignment for universal quantifier
                break;
            }
        }

        certificate
    }
}

impl<'a> super::Solver for CaqeSolver<'a> {
    fn solve(&mut self) -> SolverResult {
        for ref mut abstraction in self.abstraction.iter_mut() {
            let result = abstraction.solve_recursive(self.matrix);
            if result == SolverResult::Unsatisfiable {
                self.result = SolverResult::Unsatisfiable;
                return result;
            }
        }
        self.result = SolverResult::Satisfiable;
        return self.result;
    }
}

#[derive(Debug, Copy, Clone)]
pub struct CaqeSolverOptions {
    pub strong_unsat_refinement: bool,
    pub expansion_refinement: bool,
    pub refinement_literal_subsumption: bool,
    pub abstraction_literal_optimization: bool,
    /// flag whether to collpase empty (universal) scopes duging mini-scoping
    pub collapse_empty_scopes: bool,
}

impl CaqeSolverOptions {
    pub fn new() -> CaqeSolverOptions {
        CaqeSolverOptions {
            strong_unsat_refinement: true,
            expansion_refinement: true,
            refinement_literal_subsumption: false,
            abstraction_literal_optimization: true,
            collapse_empty_scopes: false,
        }
    }
}

#[derive(PartialEq, Eq, Hash, Clone, Copy, Debug)]
enum SolverScopeEvents {
    SolveScopeAbstraction,
    Refinement,
}

impl fmt::Display for SolverScopeEvents {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            &SolverScopeEvents::SolveScopeAbstraction => write!(f, "SolveScopeAbstraction"),
            &SolverScopeEvents::Refinement => write!(f, "Refinement"),
        }
    }
}

struct ScopeSolverData {
    sat: cryptominisat::Solver,
    variables: Vec<Variable>,
    variable_to_sat: HashMap<Variable, Lit>,
    t_literals: Vec<(ClauseId, Lit)>,
    b_literals: Vec<(ClauseId, Lit)>,

    /// lookup from sat solver variables to clause id's
    reverse_t_literals: HashMap<u32, ClauseId>,

    assignments: HashMap<Variable, bool>,

    /// stores for every clause whether the clause is satisfied or not by assignments to outer variables
    entry: BitVec,

    /// Stores the clauses for which the current level is maximal, i.e.,
    /// there is no literal of a inner scope contained.
    /// For universal scopes, it stores the clauses which are only influenced by
    /// the current, or some inner, scope.
    max_clauses: BitVec,

    /// Stores the clauses which are relevant, i.e., belong to the current branch in quantifier prefix tree
    relevant_clauses: BitVec,

    /// stores the assumptions given to sat solver
    sat_solver_assumptions: Vec<Lit>,

    is_universal: bool,
    scope_id: ScopeId,

    options: CaqeSolverOptions,

    /// stores for clause-ids whether there is astrong-unsat optimized lit
    strong_unsat_cache: HashMap<ClauseId, (Lit, bool)>,
    conjunction: Vec<ClauseId>,

    /// expansion related data structures
    expansion_renaming: HashMap<Variable, Lit>,

    /// stores the result of recursive calls to branches
    sub_result: SolverResult,

    #[cfg(feature = "statistics")]
    statistics: TimingStats<SolverScopeEvents>,
}

impl ScopeSolverData {
    fn new(
        matrix: &QMatrix,
        options: CaqeSolverOptions,
        scope: &Scope,
        relevant_clauses: BitVec,
    ) -> ScopeSolverData {
        let mut s = cryptominisat::Solver::new();
        s.set_num_threads(1);
        // assign all variables initially to zero, need that for expansion refinement
        let mut assignments = HashMap::new();
        for &variable in scope.variables.iter() {
            assignments.insert(variable, false);
        }
        ScopeSolverData {
            sat: s,
            variables: scope.variables.clone(),
            variable_to_sat: HashMap::new(),
            t_literals: Vec::with_capacity(matrix.clauses.len()),
            b_literals: Vec::with_capacity(matrix.clauses.len()),
            reverse_t_literals: HashMap::new(),
            assignments: assignments,
            entry: BitVec::from_elem(matrix.clauses.len(), false),
            max_clauses: BitVec::from_elem(matrix.clauses.len(), false),
            relevant_clauses: relevant_clauses,
            sat_solver_assumptions: Vec::new(),
            is_universal: scope.id % 2 != 0,
            scope_id: scope.id,
            options: options,
            strong_unsat_cache: HashMap::new(),
            conjunction: Vec::new(),
            expansion_renaming: HashMap::new(),
            sub_result: SolverResult::Unknown,
            #[cfg(feature = "statistics")]
            statistics: TimingStats::new(),
        }
    }

    fn new_existential(&mut self, matrix: &QMatrix, scope: &Scope) {
        let mut sat_clause = Vec::new();

        // build SAT instance for existential quantifier: abstract all literals that are not contained in quantifier into b- and t-literals
        'next_clause: for (clause_id, clause) in matrix.clauses.iter().enumerate() {
            debug_assert!(clause.len() != 0, "unit clauses indicate a problem");
            debug_assert!(sat_clause.is_empty());

            let mut contains_variables = false;
            let mut outer = None;
            let mut inner = None;
            let mut current = None;
            let mut scopes = MinMax::new();

            for &literal in clause.iter() {
                let var_scope = matrix.prefix.variables().get(literal.variable()).scope;
                scopes.update(var_scope);
                if !self.variable_to_sat.contains_key(&literal.variable()) {
                    if var_scope < scope.id {
                        outer = Some(literal);
                    } else if var_scope > scope.id {
                        inner = Some(literal);
                    }
                    continue;
                }
                self.relevant_clauses.set(clause_id as usize, true);
                current = Some(literal);
                contains_variables = true;
                sat_clause.push(self.lit_to_sat_lit(literal));
            }

            // add t- and b-lits to existential quantifiers:
            // * we add t-lit if scope is between min- and max-scope of current clause
            // * we add b-lit if scope is between min- and max-scope of current clause, excluding max-scope
            let (min_scope, max_scope) = scopes.get();
            let need_t_lit = contains_variables && min_scope < scope.id && scope.id <= max_scope;
            let need_b_lit = contains_variables && min_scope <= scope.id && scope.id < max_scope;

            let mut outer_equal_to = None;
            let mut inner_equal_to = None;

            if min_scope > self.scope_id {
                // remove the clause from relevant clauses as current scope (nor any outer) influence it
                self.relevant_clauses.set(clause_id as usize, false);
            }

            if !contains_variables {
                debug_assert!(!need_t_lit);
                debug_assert!(!need_b_lit);
                debug_assert!(sat_clause.is_empty());
                continue;
            } else {
                // check if the clause is equal to another clause w.r.t. variables bound at the current level or outer
                // in this case, we do not need to add a clause to SAT solver, but rather just need an entry in b-literals
                if self.options.abstraction_literal_optimization && need_b_lit && current.is_some()
                {
                    for &other_clause_id in matrix
                        .occurrences(current.unwrap())
                        .filter(|&&id| id < clause_id as ClauseId)
                    {
                        let other_clause = &matrix.clauses[other_clause_id as usize];
                        if clause.is_equal_wrt_predicate(other_clause, |l| {
                            let info = matrix.prefix.variables().get(l.variable());
                            info.scope <= scope.id
                        }) {
                            debug_assert!(need_b_lit);
                            let pos = self.b_literals
                                .binary_search_by(|elem| elem.0.cmp(&other_clause_id));
                            if pos.is_ok() {
                                let sat_var = self.b_literals[pos.unwrap()].1;
                                self.b_literals.push((clause_id as ClauseId, sat_var));
                                sat_clause.clear();
                                continue 'next_clause;
                            }
                        }
                    }
                }
                if false && self.options.abstraction_literal_optimization && outer.is_some() {
                    for &other_clause_id in matrix
                        .occurrences(outer.unwrap())
                        .filter(|&&id| id < clause_id as ClauseId)
                    {
                        let other_clause = &matrix.clauses[other_clause_id as usize];
                        if clause.is_equal_wrt_predicate(other_clause, |l| {
                            let info = matrix.prefix.variables().get(l.variable());
                            info.scope < scope.id
                        }) {
                            debug_assert!(need_t_lit);
                            let pos = self.t_literals
                                .binary_search_by(|elem| elem.0.cmp(&other_clause_id));
                            if pos.is_ok() {
                                let sat_var = self.t_literals[pos.unwrap()].1;
                                outer_equal_to = Some(sat_var);
                                break;
                            }
                        }
                    }
                }
                if false && self.options.abstraction_literal_optimization && inner.is_some() {
                    for &other_clause_id in matrix
                        .occurrences(inner.unwrap())
                        .filter(|&&id| id < clause_id as ClauseId)
                    {
                        let other_clause = &matrix.clauses[other_clause_id as usize];
                        if clause.is_equal_wrt_predicate(other_clause, |l| {
                            let info = matrix.prefix.variables().get(l.variable());
                            info.scope > scope.id
                        }) {
                            debug_assert!(need_b_lit);
                            let pos = self.b_literals
                                .binary_search_by(|elem| elem.0.cmp(&other_clause_id));
                            if pos.is_ok() {
                                let sat_var = self.b_literals[pos.unwrap()].1;
                                inner_equal_to = Some(sat_var);
                                break;
                            }
                        }
                    }
                }
            }

            if need_t_lit {
                if outer_equal_to.is_none() {
                    let t_lit = self.sat.new_var();
                    sat_clause.push(t_lit);
                    self.t_literals.push((clause_id as ClauseId, t_lit));
                    self.reverse_t_literals
                        .insert(t_lit.var(), clause_id as ClauseId);
                } else {
                    let t_lit = outer_equal_to.unwrap();
                    sat_clause.push(t_lit);
                    // we don't need to add it to t-literals since it will be already assumed by earlier clause
                    // otherwise, we would assume twice
                    //self.t_literals.push((clause_id as ClauseId, t_lit));
                }
            }

            if need_b_lit {
                let b_lit;
                if inner_equal_to.is_none() {
                    b_lit = self.sat.new_var();
                } else {
                    b_lit = inner_equal_to.unwrap();
                }
                sat_clause.push(!b_lit);
                self.b_literals.push((clause_id as ClauseId, b_lit));
            }

            debug_assert!(!sat_clause.is_empty());
            self.sat.add_clause(sat_clause.as_ref());
            sat_clause.clear();

            if max_scope == scope.id {
                self.max_clauses.set(clause_id, true);
            }
        }

        debug!("Scope {}", scope.id);
        debug!("t-literals: {}", self.t_literals.len());
        debug!("b-literals: {}", self.b_literals.len());

        #[cfg(debug_assertions)]
        {
            let mut t_literals = String::new();
            for &(clause_id, _) in self.t_literals.iter() {
                t_literals.push_str(&format!(" t{}", clause_id));
            }
            debug!("t-literals: {}", t_literals);

            let mut b_literals = String::new();
            for &(clause_id, _) in self.b_literals.iter() {
                b_literals.push_str(&format!(" b{}", clause_id));
            }
            debug!("b-literals: {}", b_literals);
        }
    }

    fn new_universal(&mut self, matrix: &QMatrix, scope: &Scope) {
        // build SAT instance for negation of clauses, i.e., basically we only build binary clauses
        'next_clause: for (clause_id, clause) in matrix.clauses.iter().enumerate() {
            debug_assert!(clause.len() != 0, "unit clauses indicate a problem");

            let clause_id = clause_id as ClauseId;

            let mut scopes = MinMax::new();

            // check if there is at most one variable bound in current scope (and no outer variables)
            // then one can replace the b-literal by the variable itself
            let mut single_literal = None;
            let mut num_scope_variables = 0;
            for &literal in clause.iter() {
                let var_scope = matrix.prefix.variables().get(literal.variable()).scope;
                scopes.update(var_scope);
                if !self.variable_to_sat.contains_key(&literal.variable()) {
                    continue;
                }
                self.relevant_clauses.set(clause_id as usize, true);
                num_scope_variables += 1;
                if single_literal.is_none() {
                    single_literal = Some(literal);
                }
            }
            let (min_scope, max_scope) = scopes.get();

            // We check whether the clause is equal to a prior clause w.r.t. outer and current variables.
            // In this case, we can re-use the b-literal from other clause (and can omit t-literal all together).
            if self.options.abstraction_literal_optimization && single_literal.is_some()
                && (num_scope_variables > 1 || min_scope < scope.id)
            {
                let literal = single_literal.unwrap();
                // iterate only over prior clauses
                for &other_clause_id in matrix
                    .occurrences(literal)
                    .filter(|&&id| id < clause_id as ClauseId)
                {
                    let other_clause = &matrix.clauses[other_clause_id as usize];
                    if clause.is_equal_wrt_predicate(other_clause, |l| {
                        let info = matrix.prefix.variables().get(l.variable());
                        info.scope <= scope.id
                    }) {
                        let pos = self.b_literals
                            .binary_search_by(|elem| elem.0.cmp(&other_clause_id))
                            .unwrap();
                        let sat_var = self.b_literals[pos].1;
                        self.b_literals.push((clause_id as ClauseId, sat_var));
                        continue 'next_clause;
                    }
                }
            }

            let sat_var;

            // there is a single literal and no outer variables, replace t-literal by literal
            if self.options.abstraction_literal_optimization && num_scope_variables == 1
                && min_scope == scope.id
            {
                let literal = single_literal.unwrap();
                sat_var = !self.lit_to_sat_lit(literal);
            } else if num_scope_variables > 0 {
                // build abstraction
                sat_var = self.sat.new_var();
                for &literal in clause.iter() {
                    if !self.variable_to_sat.contains_key(&literal.variable()) {
                        continue;
                    }
                    let lit = self.lit_to_sat_lit(literal);
                    self.sat.add_clause(&[!sat_var, !lit]);
                }
            } else {
                // no variable of current scope
                // do not add t-literal nor b-literal, we adapt abstraction during solving if needed
                continue;
            }

            debug_assert!(self.relevant_clauses[clause_id as usize]);

            let need_t_lit = min_scope < scope.id && scope.id <= max_scope;
            let need_b_lit = min_scope <= scope.id && scope.id <= max_scope;

            debug_assert!(min_scope <= scope.id);
            debug_assert!(max_scope >= scope.id);

            if need_t_lit {
                self.t_literals.push((clause_id as ClauseId, sat_var));
                debug_assert!(!self.reverse_t_literals.contains_key(&sat_var.var()));
                self.reverse_t_literals
                    .insert(sat_var.var(), clause_id as ClauseId);
            }

            if need_b_lit {
                self.b_literals.push((clause_id as ClauseId, sat_var));
            }

            if min_scope == scope.id {
                self.max_clauses.set(clause_id as usize, true);
            }
        }

        debug!("Scope {}", scope.id);
        debug!("t-literals: {}", self.t_literals.len());
        debug!("b-literals: {}", self.b_literals.len());

        #[cfg(debug_assertions)]
        {
            let mut t_literals = String::new();
            for &(clause_id, _) in self.t_literals.iter() {
                t_literals.push_str(&format!(" t{}", clause_id));
            }
            debug!("t-literals: {}", t_literals);

            let mut b_literals = String::new();
            for &(clause_id, _) in self.b_literals.iter() {
                b_literals.push_str(&format!(" b{}", clause_id));
            }
            debug!("b-literals: {}", b_literals);
        }
    }

    fn lit_to_sat_lit(&self, literal: Literal) -> Lit {
        let lit = self.variable_to_sat[&literal.variable()];
        if literal.signed() {
            !lit
        } else {
            lit
        }
    }

    fn check_candidate_exists(&mut self, next: &mut Vec<Box<ScopeRecursiveSolver>>) -> Lbool {
        // we need to reset abstraction entries for next scopes, since some entries may be pushed down
        self.entry.intersect(&self.relevant_clauses);
        for ref mut scope in next {
            scope.data.entry.clone_from(&self.entry);
        }

        self.sat_solver_assumptions.clear();

        #[cfg(debug_assertions)]
        let mut debug_print = String::new();

        // we iterate in parallel over the entry and the t-literals of current level
        // there are 3 possibilities:
        // * clause from entry is not a t-lit: push entry to next quantifier
        // * clause is in entry and a t-lit: assume positively
        // * clause is not in entry and a t-lit: assume negatively
        for &(clause_id, mut t_literal) in self.t_literals.iter() {
            if !self.entry[clause_id as usize] {
                t_literal = !t_literal;
            }
            if self.is_universal {
                t_literal = !t_literal;
            }

            #[cfg(debug_assertions)]
            {
                if t_literal.isneg() {
                    debug_print.push_str(&format!(" -t{}", clause_id));
                } else {
                    debug_print.push_str(&format!(" t{}", clause_id));
                }
            }

            if self.is_universal && !t_literal.isneg() {
                // assume t-literal completely for existential quantifier
                // and only negatively for universal quantifier
                continue;
            }

            self.sat_solver_assumptions.push(t_literal);
        }

        #[cfg(debug_assertions)]
        debug!("assume {}", debug_print);

        self.sat
            .solve_with_assumptions(self.sat_solver_assumptions.as_ref())
    }

    fn update_assignment(&mut self) {
        trace!("update_assignment");

        #[cfg(debug_assertions)]
        let mut debug_print = String::new();

        let model = self.sat.get_model();
        for (&variable, &sat_var) in self.variable_to_sat.iter() {
            let value = match model[sat_var.var() as usize] {
                Lbool::True => true,
                Lbool::False => false,
                _ => panic!("expect all variables to be assigned"),
            };

            #[cfg(debug_assertions)]
            {
                if value {
                    debug_print.push_str(&format!(" {}", variable));
                } else {
                    debug_print.push_str(&format!(" -{}", variable));
                }
            }

            let old = self.assignments.entry(variable).or_insert(value);
            *old = value;
        }

        #[cfg(debug_assertions)]
        debug!("assignment {}", debug_print);
    }

    fn get_assumptions(&mut self, matrix: &QMatrix, next: &mut Vec<Box<ScopeRecursiveSolver>>) {
        trace!("get_assumptions");

        // assumptions in `next` were already cleared in check_candidate_exists

        #[cfg(debug_assertions)]
        let mut debug_print = String::new();

        if !self.is_universal {
            for &(clause_id, b_lit) in self.b_literals.iter() {
                if self.sat.is_true(b_lit) {
                    next.iter_mut().for_each(|ref mut scope| {
                        scope.data.entry.set(clause_id as usize, true);
                    });
                    continue;
                }
                /*debug_assert!(
                    !self.entry[clause_id as usize] || assumptions[clause_id as usize],
                    "entry -> assumption"
                );*/

                if self.entry[clause_id as usize] {
                    //debug_assert!(assumptions[clause_id as usize]);
                    continue;
                }

                // assumption literal was set, but it may be still true that the clause is satisfied
                let clause = &matrix.clauses[clause_id as usize];
                if clause.is_satisfied_by_assignment(&self.assignments) {
                    next.iter_mut().for_each(|ref mut scope| {
                        scope.data.entry.set(clause_id as usize, true);
                    });
                    continue;
                }

                #[cfg(debug_assertions)]
                debug_print.push_str(&format!(" b{}", clause_id));
            }
        } else {
            for &(clause_id, b_lit) in self.b_literals.iter() {
                if self.sat.is_true(b_lit) {
                    continue;
                }

                // assumption literal was set
                // check if clause is falsified by current level
                let clause = &matrix.clauses[clause_id as usize];
                let mut falsified = true;
                let mut nonempty = false;

                for literal in clause.iter() {
                    if !self.variable_to_sat.contains_key(&literal.variable()) {
                        // not a variable of current level
                        continue;
                    }
                    nonempty = true;
                    let value = self.assignments[&literal.variable()];
                    if value && !literal.signed() {
                        falsified = false;
                        break;
                    } else if !value && literal.signed() {
                        falsified = false;
                        break;
                    }
                }
                if nonempty && falsified {
                    // depending on t-literal, the assumption is already set
                    continue;
                    /*if self.t_literals.contains_key(&clause_id) {
                        if !self.entry[clause_id as usize] {
                            continue;
                        }
                    } else {
                        continue;
                    }*/
                }
                if !nonempty {
                    debug_assert!(
                        self.t_literals
                            .binary_search_by(|elem| elem.0.cmp(&clause_id))
                            .is_ok()
                    );
                    // we have already copied the value by copying current entry
                    continue;
                    /*if !self.entry[clause_id as usize] {
                        continue;
                    }*/
                }

                next.iter_mut().for_each(|ref mut scope| {
                    scope.data.entry.set(clause_id as usize, true);
                });

                #[cfg(debug_assertions)]
                debug_print.push_str(&format!(" b{}", clause_id));
            }
        }

        #[cfg(debug_assertions)]
        debug!("assumptions: {}", debug_print);
    }

    fn entry_minimization(&mut self, matrix: &QMatrix) {
        trace!("entry_minimization");

        // add clauses to entry where the current scope is maximal
        self.entry.union(&self.max_clauses);

        for variable in self.variables.iter() {
            let value = self.assignments[variable];
            let literal = Literal::new(*variable, !value);

            // check if assignment is needed, i.e., it can flip a bit in entry
            let mut needed = false;
            for &clause_id in matrix.occurrences(literal) {
                if self.entry[clause_id as usize] {
                    needed = true;
                    self.entry.set(clause_id as usize, false);
                }
            }

            if !needed {
                // the current value set is not needed for the entry, try other polarity
                for &clause_id in matrix.occurrences(-literal) {
                    if self.entry[clause_id as usize] {
                        self.entry.set(clause_id as usize, false);
                    }
                }
            }
        }

        #[cfg(debug_assertions)]
        for (i, val) in self.entry.iter().enumerate().filter(|&(_, val)| val) {
            let clause = &matrix.clauses[i];
            let mut min = ScopeId::max_value();
            for &literal in clause.iter() {
                let otherscope = matrix.prefix.variables().get(literal.variable()).scope;
                if otherscope < min {
                    min = otherscope;
                }
            }
            assert!(min < self.scope_id);
        }
    }

    fn refine(&mut self, matrix: &QMatrix, next: &mut Box<ScopeRecursiveSolver>) {
        trace!("refine");

        // check if influenced by current scope
        /*let mut max = 0;
        for (i, _) in self.entry.iter().enumerate().filter(|&(_, val)| val) {
            let clause = &matrix.clauses[i];

            for &literal in clause.iter() {
                let otherscope = matrix.prefix.variables().get(literal.variable()).scope;
                if otherscope > self.scope_id {
                    continue;
                }
                if otherscope > max {
                    max = otherscope;
                }
            }
        }
        if max < self.scope_id && self.scope_id > 1 {
            println!("{} {}", max, self.scope_id);
            panic!("a");
        }*/

        if self.options.expansion_refinement && self.is_expansion_refinement_applicable(next) {
            self.expansion_refinement(matrix, next);
        }

        if !self.is_universal && self.options.strong_unsat_refinement
            && self.strong_unsat_refinement(matrix, next)
        {
            return;
        }
        // important: refinement literal subsumption has to be after strong unsat refinement
        if self.options.refinement_literal_subsumption {
            self.refinement_literal_subsumption_optimization(matrix, next);
        }

        let entry = &next.data.entry;
        let blocking_clause = &mut self.sat_solver_assumptions;
        blocking_clause.clear();

        #[cfg(debug_assertions)]
        let mut debug_print = String::new();

        for (i, _) in entry.iter().enumerate().filter(|&(_, val)| val) {
            let clause_id = i as ClauseId;
            let b_lit = Self::add_b_lit_and_adapt_abstraction(
                clause_id,
                &mut self.sat,
                &self.b_literals,
                &mut self.t_literals,
                &mut self.reverse_t_literals,
            );
            blocking_clause.push(b_lit);

            #[cfg(debug_assertions)]
            debug_print.push_str(&format!(" b{}", clause_id));
        }
        self.sat.add_clause(blocking_clause.as_ref());

        #[cfg(debug_assertions)]
        debug!("refinement: {}", debug_print);
    }

    /// Implements the strong unsat refinement operation.
    /// If successful, it can reduce the number of iterations needed.
    /// Returns true, if the optimization was applied, false otherwise.
    fn strong_unsat_refinement(
        &mut self,
        matrix: &QMatrix,
        next: &mut Box<ScopeRecursiveSolver>,
    ) -> bool {
        trace!("strong_unsat_refinement");
        debug_assert!(!self.is_universal);
        let mut applied = false;

        // re-use sat-solver-assumptions vector
        let blocking_clause = &mut self.sat_solver_assumptions;
        blocking_clause.clear();

        let entry = &next.data.entry;
        let scope_id = self.scope_id;

        // was the clause processed before?
        for (i, _) in entry.iter().enumerate().filter(|&(_, val)| val) {
            let clause_id = i as ClauseId;
            match self.strong_unsat_cache.get(&clause_id) {
                Some(&(literal, opt)) => {
                    if opt {
                        applied = true;
                    }
                    blocking_clause.push(literal);
                    continue;
                }
                None => {}
            }

            // TODO: for implementation of stronger unsat rule (see "On Expansion and Resolution in CEGAR Based QBF Solving"),
            // we have to collect all universal variables from all failed clauses.
            // This means espacially that we cannot use our current hashing anymore

            // Get some random existential occurrence from clause, so we can use
            // the occurrence list to iterate over clauses
            let clause = &matrix.clauses[i];
            self.conjunction.clear();
            self.conjunction.push(clause_id);
            for &literal in clause.iter() {
                let info = matrix.prefix.variables().get(literal.variable());

                // Consider only existential variables that have a lower level
                if info.is_universal() || info.scope <= self.scope_id {
                    continue;
                }

                // Iterate over occurrence list and add equivalent clauses
                for &other_clause_id in matrix.occurrences(literal) {
                    let other_clause = &matrix.clauses[other_clause_id as usize];
                    // check if clause is subset w.r.t. inner variables
                    if clause_id != other_clause_id
                        && self.relevant_clauses[other_clause_id as usize]
                    {
                        let pos = match self.conjunction.binary_search(&other_clause_id) {
                            Ok(_) => continue, // already contained, skip
                            Err(pos) => pos,   // position to insert
                        };

                        if other_clause.is_subset_wrt_predicate(clause, |l| {
                            matrix.prefix.variables().get(l.variable()).scope > scope_id
                        }) {
                            debug_assert!(!self.max_clauses[other_clause_id as usize]);
                            self.conjunction.insert(pos, other_clause_id);
                        }
                    }
                }
            }

            debug_assert!(self.conjunction.len() > 0);
            if self.conjunction.len() == 1 {
                // do not need auxilliary variable
                let clause_id = self.conjunction[0];
                let sat_lit = Self::add_b_lit_and_adapt_abstraction(
                    clause_id,
                    &mut self.sat,
                    &self.b_literals,
                    &mut self.t_literals,
                    &mut self.reverse_t_literals,
                );
                blocking_clause.push(sat_lit);
                self.strong_unsat_cache.insert(clause_id, (sat_lit, false));
            } else {
                // build the conjunction using an auxilliary variable
                let aux_var = self.sat.new_var();
                blocking_clause.push(aux_var);
                self.strong_unsat_cache.insert(clause_id, (aux_var, true));

                for &other_clause_id in self.conjunction.iter() {
                    let sat_lit = Self::add_b_lit_and_adapt_abstraction(
                        other_clause_id,
                        &mut self.sat,
                        &self.b_literals,
                        &mut self.t_literals,
                        &mut self.reverse_t_literals,
                    );
                    self.sat.add_clause(&[!aux_var, sat_lit]);
                }
                applied = true;
            }
        }

        if applied {
            self.sat.add_clause(blocking_clause.as_ref());
        }

        applied
    }

    /// Tries to reduce the size of refinements.
    /// If a clause is subsumed by another clause in refinement, it can be removed.
    /// This does not change the number of iterations, but may make the job of SAT solver easier.
    ///
    /// Returns true if the refinement clause could be reduced.
    fn refinement_literal_subsumption_optimization(
        &mut self,
        matrix: &QMatrix,
        next: &mut Box<ScopeRecursiveSolver>,
    ) -> bool {
        let mut successful = false;
        let entry = &mut next.data.entry;
        'outer: for i in 0..entry.len() {
            if !entry[i] {
                continue;
            }
            let clause_id = i as ClauseId;
            let clause = &matrix.clauses[i];
            for &literal in clause.iter() {
                let info = matrix.prefix.variables().get(literal.variable());
                if info.scope > self.scope_id {
                    // do not consider inner variables
                    continue;
                }
                // iterate over occurrence list
                for &other_clause_id in matrix.occurrences(literal) {
                    if clause_id == other_clause_id {
                        continue;
                    }
                    if !entry[other_clause_id as usize] {
                        // not in entry, thus not interesting
                        continue;
                    }
                    let other_clause = &matrix.clauses[other_clause_id as usize];
                    let current_scope = self.scope_id;
                    // check if other clause subsumes current
                    // check is done with respect to current and outer variables
                    if self.is_universal {
                        if other_clause.is_subset_wrt_predicate(clause, |l| {
                            let info = matrix.prefix.variables().get(l.variable());
                            info.scope <= current_scope
                        }) {
                            entry.set(clause_id as usize, false);
                            successful = true;
                            continue 'outer;
                        }
                    } else {
                        if clause.is_subset_wrt_predicate(other_clause, |l| {
                            let info = matrix.prefix.variables().get(l.variable());
                            info.scope <= current_scope
                        }) {
                            entry.set(clause_id as usize, false);
                            successful = true;
                            continue 'outer;
                        }
                    }
                }
            }
        }
        successful
    }

    fn is_expansion_refinement_applicable(&self, next: &mut Box<ScopeRecursiveSolver>) -> bool {
        if self.is_universal {
            return false;
        }
        //return true;
        debug_assert!(next.next.len() == 1);
        return next.next[0].as_ref().next.is_empty();
    }

    fn expansion_refinement(&mut self, matrix: &QMatrix, next: &mut Box<ScopeRecursiveSolver>) {
        trace!("expansion_refinement");
        let universal_assignment = next.get_universal_assignmemnt(HashMap::new());
        let (data, next) = next.split();
        let next = &next[0];

        // add a new sat variable for every existential variable in inner scope (updated lazily)
        self.expansion_renaming.clear();

        let sat = &mut self.sat;
        let sat_clause = &mut self.sat_solver_assumptions;

        // create the refinement clauses
        for (i, clause) in matrix.clauses.iter().enumerate() {
            if !next.data.relevant_clauses[i] {
                continue;
            }

            // check if the universal assignment satisfies the clause
            if clause.is_satisfied_by_assignment(&universal_assignment) {
                continue;
            }

            sat_clause.clear();

            // add the clause to the abstraction
            // variables bound by inner existential quantifier have to be renamed
            let mut contains_variables = false;
            let mut contains_outer_variables = false;
            for &literal in clause.iter() {
                let info = matrix.prefix.variables().get(literal.variable());
                if info.scope <= data.scope_id {
                    if info.scope < self.scope_id {
                        contains_outer_variables = true;
                    }
                    continue;
                }
                if info.scope % 2 == 1 {
                    debug_assert!(universal_assignment.contains_key(&literal.variable()));
                    continue;
                }
                debug_assert!(info.scope > self.scope_id);
                contains_variables = true;

                let entry = self.expansion_renaming
                    .entry(literal.variable())
                    .or_insert_with(|| sat.new_var());
                let mut sat_var = *entry;
                if literal.signed() {
                    sat_var = !sat_var;
                }
                sat_clause.push(sat_var);
            }
            let clause_id = i as ClauseId;
            if self.b_literals
                .binary_search_by(|elem| elem.0.cmp(&clause_id))
                .is_ok() || contains_variables && contains_outer_variables
            {
                let sat_lit = Self::add_b_lit_and_adapt_abstraction(
                    clause_id,
                    sat,
                    &self.b_literals,
                    &mut self.t_literals,
                    &mut self.reverse_t_literals,
                );
                sat_clause.push(sat_lit);
            }
            if !contains_variables {
                continue;
            }
            if !sat_clause.is_empty() {
                sat.add_clause(sat_clause.as_ref());
            }
        }
    }

    fn add_b_lit_and_adapt_abstraction(
        clause_id: ClauseId,
        sat: &mut cryptominisat::Solver,
        b_literals: &Vec<(ClauseId, Lit)>,
        t_literals: &mut Vec<(ClauseId, Lit)>,
        reverse_t_literals: &mut HashMap<u32, Variable>,
    ) -> Lit {
        // first check if there is a b-literal for clause
        // if yes, just return it (the currents scope influences clause since there is at least one variable contained)
        // if no, we continue
        match b_literals.binary_search_by(|elem| elem.0.cmp(&clause_id)) {
            Ok(pos) => return b_literals[pos].1,
            Err(_) => {}
        };

        // we then check, if there is a corresponding t-literal
        // if yes, we return this instead
        // if no, we have to adapt the abstraction by inserting a new t-literal
        let insert_pos = match t_literals.binary_search_by(|elem| elem.0.cmp(&clause_id)) {
            Ok(pos) => return t_literals[pos].1,
            Err(pos) => pos,
        };
        let sat_lit = sat.new_var();
        t_literals.insert(insert_pos, (clause_id, sat_lit));
        reverse_t_literals.insert(sat_lit.var(), clause_id);

        // note that, we could also adapt b_literals (with the same sat_literal)
        // however, this is not necessary and not directly obvious
        // 1) reason *not* to do it: in get_assumptions we iterate over b_literals to check
        //    if we can improve the assumptions produced by the SAT solver. Since the clauses
        //    that are added here have no influence of current scope, this check is wasted time
        // 2) we do not *need* them, because abstraction entries are just copied from one
        //    scope to another

        sat_lit
    }

    fn get_unsat_core(&mut self) {
        trace!("unsat_core");

        self.entry.clear();

        #[cfg(debug_assertions)]
        let mut debug_print = String::new();

        let failed = self.sat.get_conflict();
        for l in failed {
            let clause_id = self.reverse_t_literals[&l.var()];
            self.entry.set(clause_id as usize, true);

            #[cfg(debug_assertions)]
            debug_print.push_str(&format!(" t{}", clause_id));
        }

        #[cfg(debug_assertions)]
        debug!("unsat core: {}", debug_print);
    }

    /// filters those clauses that are only influenced by this quantifier (or inner)
    fn unsat_propagation(&mut self) {
        self.entry.difference(&self.max_clauses);
    }
}

struct ScopeRecursiveSolver {
    data: ScopeSolverData,
    next: Vec<Box<ScopeRecursiveSolver>>,
}

impl ScopeRecursiveSolver {
    fn new(
        matrix: &QMatrix,
        options: CaqeSolverOptions,
        scope: &Scope,
        quantifier: Quantifier,
        next: Vec<Box<ScopeRecursiveSolver>>,
    ) -> ScopeRecursiveSolver {
        let mut relevant_clauses = BitVec::from_elem(matrix.clauses.len(), false);
        for ref next_scope in next.iter() {
            #[cfg(debug_assertions)]
            {
                // the branches have pairwise disjoint relevant clauses
                let mut copy = relevant_clauses.clone();
                copy.intersect(&next_scope.data.relevant_clauses);
                assert!(copy.none());
            }
            relevant_clauses.union(&next_scope.data.relevant_clauses);
        }
        let mut candidate = ScopeRecursiveSolver {
            data: ScopeSolverData::new(matrix, options, scope, relevant_clauses),
            next: next,
        };

        // add variables of scope to sat solver
        for &variable in scope.variables.iter() {
            candidate
                .data
                .variable_to_sat
                .insert(variable, candidate.data.sat.new_var());
        }

        match quantifier {
            Quantifier::Existential => candidate.data.new_existential(matrix, scope),
            Quantifier::Universal => candidate.data.new_universal(matrix, scope),
        }

        candidate
    }

    fn init_abstraction_recursively(
        matrix: &QMatrix,
        options: CaqeSolverOptions,
        scope_node: &Box<ScopeNode>,
    ) -> Box<ScopeRecursiveSolver> {
        let mut prev = Vec::new();
        for ref child_node in scope_node.next.iter() {
            prev.push(Self::init_abstraction_recursively(
                matrix,
                options.clone(),
                child_node,
            ))
        }
        let scope = &scope_node.scope;
        let result = Box::new(ScopeRecursiveSolver::new(
            matrix,
            options,
            scope,
            Quantifier::from(scope.id),
            prev,
        ));

        #[cfg(debug_assertions)]
        {
            // check consistency of interface literals
            // for every b_lit in abstraction, there is a corresponding t_lit in one of its inner abstractions
            /*for &(clause_id, _b_lit) in result.data.b_literals.iter() {
                let mut current = &result;
                let mut found = false;
                while let Some(next) = current.next.as_ref() {
                    if next.data
                        .t_literals
                        .binary_search_by(|elem| elem.0.cmp(&clause_id))
                        .is_ok()
                    {
                        found = true;
                        break;
                    }
                    current = &next;
                }
                if !found {
                    panic!(
                        "missing t-literal for b-literal {} at scope {}",
                        clause_id, scope.id
                    );
                }
            }*/

            /*if scope_id == 0 {
                let mut abstractions = Vec::new();
                Self::verify_t_literals(&mut abstractions, result.as_ref());
            }*/
        }

        result
    }

    /*#[cfg(debug_assertions)]
    fn verify_t_literals<'a>(
        abstractions: &mut Vec<&'a ScopeRecursiveSolver>,
        scope: &'a ScopeRecursiveSolver,
    ) {
        // check that for every clause containing a t-literal at this scope,
        // there is a clause containing a b-literal in the previous scope
        abstractions.push(scope);
        for next in scope.next {
            for &(clause_id, _t_lit) in next.data.t_literals.iter() {
                let has_matching_b_lit = abstractions.iter().fold(false, |val, &abstraction| {
                    val
                        || abstraction
                            .data
                            .b_literals
                            .binary_search_by(|elem| elem.0.cmp(&clause_id))
                            .is_ok()
                });
                if !has_matching_b_lit {
                    panic!(
                        "missing b-literal for t-literal {} at scope {}",
                        clause_id, next.data.scope_id
                    );
                }
            }

            Self::verify_t_literals(abstractions, next.as_ref());
        }
        abstractions.pop();
    }*/

    fn solve_recursive(&mut self, matrix: &QMatrix) -> SolverResult {
        trace!("solve_recursive");

        // mutable split
        let current = &mut self.data;
        let next = &mut self.next;

        let good_result = if current.is_universal {
            SolverResult::Unsatisfiable
        } else {
            SolverResult::Satisfiable
        };
        let bad_result = if current.is_universal {
            SolverResult::Satisfiable
        } else {
            SolverResult::Unsatisfiable
        };
        debug_assert!(good_result != bad_result);

        loop {
            debug!("");
            info!("solve level {}", current.scope_id);

            #[cfg(feature = "statistics")]
            let mut timer = current
                .statistics
                .start(SolverScopeEvents::SolveScopeAbstraction);

            match current.check_candidate_exists(next) {
                Lbool::True => {
                    // there is a candidate solution, verify it recursively
                    current.update_assignment();

                    if next.is_empty() {
                        // innermost scope, propagate result to outer scopes
                        debug_assert!(!current.is_universal);
                        //current.entry.clear();
                        current.entry_minimization(matrix);
                        return good_result;
                    }

                    current.get_assumptions(matrix, next);

                    #[cfg(feature = "statistics")]
                    timer.stop();

                    current.sub_result = good_result;
                    for ref mut scope in next.iter_mut() {
                        let result = scope.solve_recursive(matrix);
                        if result == bad_result {
                            debug_assert!(result == bad_result);
                            current.sub_result = bad_result;

                            #[cfg(feature = "statistics")]
                            let mut _timer =
                                current.statistics.start(SolverScopeEvents::Refinement);

                            current.refine(matrix, scope);
                        }
                    }

                    if current.sub_result == bad_result {
                        continue;
                    } else {
                        // copy entries from inner quantifier
                        current.entry.clear();
                        for ref scope in next.iter() {
                            current.entry.union(&scope.data.entry);
                        }
                        // apply entry optimization
                        if current.is_universal {
                            current.unsat_propagation();
                        } else {
                            current.entry_minimization(matrix);
                        }
                        return good_result;
                    }
                }
                Lbool::False => {
                    // there is no candidate solution, return witness
                    current.get_unsat_core();
                    return bad_result;
                }
                _ => panic!("inconsistent internal state"),
            }
        }
    }

    #[cfg(feature = "statistics")]
    pub fn print_statistics(&self) {
        println!("level {}", self.data.scope_id);
        self.data.statistics.print();
        for ref next in self.next.iter() {
            next.print_statistics()
        }
    }

    fn split(&mut self) -> (&mut ScopeSolverData, &mut Vec<Box<ScopeRecursiveSolver>>) {
        (&mut self.data, &mut self.next)
    }

    fn get_universal_assignmemnt(
        &self,
        mut assignment: HashMap<Variable, bool>,
    ) -> HashMap<Variable, bool> {
        if self.data.is_universal {
            assignment.extend(self.data.assignments.iter());
        }
        for ref next in self.next.iter() {
            assignment = next.get_universal_assignmemnt(assignment);
        }
        assignment
    }
}

struct MinMax {
    min: Option<i32>,
    max: Option<i32>,
}

impl MinMax {
    fn new() -> MinMax {
        MinMax {
            min: None,
            max: None,
        }
    }

    fn update(&mut self, value: i32) {
        match (self.min, self.max) {
            (None, None) => {
                self.min = Some(value);
                self.max = Some(value);
            }
            (Some(min), Some(max)) => {
                if value < min {
                    self.min = Some(value);
                }
                if value > max {
                    self.max = Some(value);
                }
            }
            _ => panic!("inconsistent internal state"),
        }
    }

    fn min(&self) -> i32 {
        self.min.unwrap()
    }

    fn max(&self) -> i32 {
        self.max.unwrap()
    }

    fn get(&self) -> (i32, i32) {
        (self.min(), self.max())
    }
}

#[cfg(test)]
mod tests {

    use super::*;
    use solver::Solver;

    #[test]
    fn test_false() {
        let instance = "p cnf 0 1\n0\n";
        let matrix = qdimacs::parse(&instance).unwrap();
        assert!(matrix.conflict());
    }

    #[test]
    fn test_true() {
        let instance = "p cnf 0 0";
        let matrix = qdimacs::parse(&instance).unwrap();
        let matrix = Matrix::unprenex_by_miniscoping(matrix, false);
        let mut solver = CaqeSolver::new(&matrix);
        assert_eq!(solver.solve(), SolverResult::Satisfiable);
        assert_eq!(solver.qdimacs_output().dimacs(), "s cnf 1 0 0\n");
    }

    #[test]
    fn test_sat_simple() {
        let instance = "c
p cnf 4 4
a 1 2 0
e 3 4 0
1 3 0
-1 4 0
-3 -4 0
-1 2 4 0
";
        let matrix = qdimacs::parse(&instance).unwrap();
        let matrix = Matrix::unprenex_by_miniscoping(matrix, false);
        let mut solver = CaqeSolver::new(&matrix);
        assert_eq!(solver.solve(), SolverResult::Satisfiable);
        assert_eq!(solver.qdimacs_output().dimacs(), "s cnf 1 4 4\n");
    }

    #[test]
    fn test_unsat_simple() {
        let instance = "c
p cnf 4 4
a 1 2 0
e 3 4 0
1 3 0
-1 4 0
-3 -4 0
1 2 4 0
";
        let matrix = qdimacs::parse(&instance).unwrap();
        let matrix = Matrix::unprenex_by_miniscoping(matrix, false);
        let mut solver = CaqeSolver::new(&matrix);
        assert_eq!(solver.solve(), SolverResult::Unsatisfiable);
        assert_eq!(
            solver.qdimacs_output().dimacs(),
            "s cnf 0 4 4\nV -1 0\nV -2 0\n"
        );
    }

    #[test]
    fn test_two_alternations() {
        let instance = "c
p cnf 11 24
a 1 0
e 2 0
a 3 0
e 4 5 6 7 8 9 10 11 0
3 5 0
-4 5 0
-3 4 -5 0
-3 6 0
4 6 0
3 -4 -6 0
2 -7 0
5 -7 0
6 -7 0
-2 -5 -6 7 0
-1 8 0
-7 8 0
1 7 -8 0
-2 -9 0
5 -9 0
6 -9 0
2 -5 -6 9 0
1 10 0
-9 10 0
-1 9 -10 0
8 -11 0
10 -11 0
-8 -10 11 0
11 0
";
        let matrix = qdimacs::parse(&instance).unwrap();
        let matrix = Matrix::unprenex_by_miniscoping(matrix, false);
        let mut solver = CaqeSolver::new(&matrix);
        assert_eq!(solver.solve(), SolverResult::Satisfiable);
        assert_eq!(solver.qdimacs_output().dimacs(), "s cnf 1 11 24\n");
    }

    #[test]
    fn test_wrong_sat() {
        let instance = "c
c This instance was falsly characterized as SAT
p cnf 4 3
a 4 0
e 3 0
a 1 0
e 2 0
-3 0
3 -4 0
-2 -1 0
";
        let matrix = qdimacs::parse(&instance).unwrap();
        let matrix = Matrix::unprenex_by_miniscoping(matrix, false);
        let mut solver = CaqeSolver::new(&matrix);
        assert_eq!(solver.solve(), SolverResult::Unsatisfiable);
        assert_eq!(solver.qdimacs_output().dimacs(), "s cnf 0 4 3\nV 4 0\n");
    }

    #[test]
    fn test_cnf() {
        let instance = "c
c CNF instance without quantifier
p cnf 1 2
-1 0
1 0
";
        let matrix = qdimacs::parse(&instance).unwrap();
        let matrix = Matrix::unprenex_by_miniscoping(matrix, false);
        let mut solver = CaqeSolver::new(&matrix);
        assert_eq!(solver.solve(), SolverResult::Unsatisfiable);
        assert_eq!(solver.qdimacs_output().dimacs(), "s cnf 0 1 2\n");
    }

    #[test]
    fn test_wrong_unsat() {
        let instance = "c
c This instance was falsly characterized as UNSAT
p cnf 3 2
a 1 2 0
e 3 0
3 -2 0
-3 -1 2 0
";
        let matrix = qdimacs::parse(&instance).unwrap();
        let matrix = Matrix::unprenex_by_miniscoping(matrix, false);
        let mut solver = CaqeSolver::new(&matrix);
        assert_eq!(solver.solve(), SolverResult::Satisfiable);
        assert_eq!(solver.qdimacs_output().dimacs(), "s cnf 1 3 2\n");
    }

    #[test]
    fn test_strong_unsat_crash() {
        let instance = "c
c This instance crashed with strong unsat refinement
p cnf 4 3
a 2 0
e 1 0
a 4 0
e 3 0
1 3 0
-3 -2 0
3 -4 0
";
        let matrix = qdimacs::parse(&instance).unwrap();
        let matrix = Matrix::unprenex_by_miniscoping(matrix, false);
        let mut solver = CaqeSolver::new(&matrix);
        assert_eq!(solver.solve(), SolverResult::Unsatisfiable);
        assert_eq!(solver.qdimacs_output().dimacs(), "s cnf 0 4 3\nV 2 0\n");
    }

    #[test]
    fn test_refinement_literal_failure() {
        let instance = "c
c This instance was solved incorrectly in earlier versions due to refinement literal optimization
p cnf 5 5
a 5 0
e 3 0
a 1 0
e 2 4 0
-2 0
4 5 0
-4 -5 0
-4 -5 -1 0
2 3 0
";
        let matrix = qdimacs::parse(&instance).unwrap();
        let matrix = Matrix::unprenex_by_miniscoping(matrix, false);
        let mut solver = CaqeSolver::new(&matrix);
        assert_eq!(solver.solve(), SolverResult::Satisfiable);
        assert_eq!(solver.qdimacs_output().dimacs(), "s cnf 1 5 5\n");
    }

    #[test]
    fn test_refinement_literal_failure2() {
        let instance = "c
c This instance was solved incorrectly in earlier versions due to refinement literal optimization
p cnf 4 3
a 4 0
e 1 0
a 3 0
e 2 0
-2 0
2 -3 -4 0
-1 -4 0
";
        let matrix = qdimacs::parse(&instance).unwrap();
        let matrix = Matrix::unprenex_by_miniscoping(matrix, false);
        let mut solver = CaqeSolver::new(&matrix);
        assert_eq!(solver.solve(), SolverResult::Unsatisfiable);
        assert_eq!(solver.qdimacs_output().dimacs(), "s cnf 0 4 3\nV 4 0\n");
    }

    #[test]
    fn test_abstraction_literal_optimization_vs_strong_unsat() {
        let instance = "c
c This instance was solved incorrectly in earlier versions due to abstraction literal optimization
p cnf 3 4
e 3 0
a 1 0
e 2 0
-2 -1 0
-2 0
-2 3 0
3 2 0
";
        let matrix = qdimacs::parse(&instance).unwrap();
        let matrix = Matrix::unprenex_by_miniscoping(matrix, false);
        let mut solver = CaqeSolver::new(&matrix);
        assert_eq!(solver.solve(), SolverResult::Satisfiable);
        assert_eq!(solver.qdimacs_output().dimacs(), "s cnf 1 3 4\nV 3 0\n");
    }

    #[test]
    fn test_strong_unsat_failure() {
        let instance = "c
c This instance was solved incorrectly in earlier versions due to strong unsat refinement.
c The strong unsat refinement can only applied to clauses which actually contains inner variables.
p cnf 4 3
e 2 3 0
a 4 0
e 1 0
-1 0
-2 3 0
3 1 -4 0
";
        let matrix = qdimacs::parse(&instance).unwrap();
        let matrix = Matrix::unprenex_by_miniscoping(matrix, false);
        let mut solver = CaqeSolver::new(&matrix);
        assert_eq!(solver.solve(), SolverResult::Satisfiable);
        assert_eq!(
            solver.qdimacs_output().dimacs(),
            "s cnf 1 4 3\nV -2 0\nV 3 0\n"
        );
    }

    #[test]
    fn test_fuzz_unsat() {
        let instance = "c
c This instance was solved incorrectly in earlier versions.
p cnf 5 5
e 1 5 0
a 4 0
e 2 3 0
-5 1 3 0
1 -5 0
-1 0
-2 4 0
5 0
";
        let matrix = qdimacs::parse(&instance).unwrap();
        let matrix = Matrix::unprenex_by_miniscoping(matrix, false);
        let mut solver = CaqeSolver::new(&matrix);
        assert_eq!(solver.solve(), SolverResult::Unsatisfiable);
        assert_eq!(solver.qdimacs_output().dimacs(), "s cnf 0 5 5\n");
    }

    #[test]
    fn test_fuzz_sat() {
        let instance = "c
c This instance was solved incorrectly in earlier versions.
p cnf 4 4
e 2 0
a 4 0
e 1 3 0
1 0
2 1 0
3 -4 0
-3 2 0
";
        let matrix = qdimacs::parse(&instance).unwrap();
        let matrix = Matrix::unprenex_by_miniscoping(matrix, false);
        let mut solver = CaqeSolver::new(&matrix);
        assert_eq!(solver.solve(), SolverResult::Satisfiable);
        assert_eq!(solver.qdimacs_output().dimacs(), "s cnf 1 4 4\nV 2 0\n");
    }

    #[test]
    fn test_wrong_unsat_miniscoping() {
        let instance = "c
c This instance was solved incorrectly in earlier versions.
p cnf 4 4
e 4 0
a 2 0
e 1 3 0
4 1 0
-1 0
4 -3 0
1 2 0
";
        let matrix = qdimacs::parse(&instance).unwrap();
        let matrix = Matrix::unprenex_by_miniscoping(matrix, false);
        let mut solver = CaqeSolver::new(&matrix);
        assert_eq!(solver.solve(), SolverResult::Unsatisfiable);
        assert_eq!(solver.qdimacs_output().dimacs(), "s cnf 0 4 4\n");
    }

    #[test]
    fn test_wrong_expansion_refinement() {
        let instance = "c
c This instance was solved incorrectly in earlier versions.
c The first conflict happens at level 2, then expansion refinement did not have universal assignments for level 3
p cnf 7 6
e 7 0
a 4 0
e 2 6 0
a 5 0
e 1 3 0
-3 5 0
3 -5 0
2 0
6 4 0
-2 7 0
-3 -2 -1 0
";
        let matrix = qdimacs::parse(&instance).unwrap();
        let matrix = Matrix::unprenex_by_miniscoping(matrix, false);
        let mut solver = CaqeSolver::new(&matrix);
        assert_eq!(solver.solve(), SolverResult::Satisfiable);
        assert_eq!(solver.qdimacs_output().dimacs(), "s cnf 1 7 6\nV 7 0\n");
    }

    #[test]
    fn test_strong_unsat_failure_2() {
        let instance = "c
c This instance was solved incorrectly in earlier versions.
p cnf 5 4
e 1 0
a 3 0
e 4 0
a 5 0
e 2 0
-2 0
-2 1 -4 3 -5 0
4 0
-4 2 1 3 0
";
        let matrix = qdimacs::parse(&instance).unwrap();
        let matrix = Matrix::unprenex_by_miniscoping(matrix, false);
        let mut solver = CaqeSolver::new(&matrix);
        assert_eq!(solver.solve(), SolverResult::Satisfiable);
        assert_eq!(solver.qdimacs_output().dimacs(), "s cnf 1 5 4\nV 1 0\n");
    }
}
