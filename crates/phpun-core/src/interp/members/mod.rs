//! Member access: shared caller-scope helpers. Sub-domains live in
//! `props` (prop access + hooks + typed slots), `methods` (method
//! dispatch + magic + visibility/prototypes), `statics` (static
//! props/calls + class consts) and `views` (object foreach/serialize
//! views, reflection, backtrace/`is_a` helpers).

use super::util::*;
use super::*;

mod methods;
mod props;
mod statics;
mod views;

pub(in crate::interp) use props::PromoteSite;

impl<'a> Interp<'a> {
    /// Name of the class whose scope the current frame runs in —
    /// private props are only visible to their own declaring class.
    /// Namespace of the currently executing code — the running
    /// function's declaring namespace, or the file-level `namespace`
    /// for top-level statements (Zend/tests/namespaces). An included
    /// file's own `namespace` governs its top-level code even when the
    /// include ran inside a function frame.
    pub fn caller_ns(&self) -> String {
        if let Some((depth, ns)) = self.include_ns.last() {
            if *depth == self.stack.len() {
                return ns.clone();
            }
        }
        self.stack
            .last()
            .map(|f| f.ns.clone())
            .unwrap_or_else(|| self.globals.ns.clone())
    }

    /// The class whose scope the caller runs in — read from the
    /// live call frame, or the globals pseudo-frame at top level,
    /// which frame-less fast paths (ctor_leaf) lend the callee's
    /// scope.
    pub(in crate::interp) fn caller_scope(&self) -> Option<&Rc<PhpClass>> {
        let f = self
            .stack
            .last()
            .map(|f| f.as_ref())
            .unwrap_or(&self.globals);
        f.decl_class.as_ref().or(f.scope_class.as_ref())
    }

    pub fn caller_scope_name(&self) -> Option<String> {
        self.caller_scope().map(|c| c.name().to_string())
    }
}
