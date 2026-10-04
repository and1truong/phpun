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

impl<'a> Interp<'a> {
    /// Name of the class whose scope the current frame runs in —
    /// private props are only visible to their own declaring class.
    /// Namespace of the currently executing code — the running
    /// function's declaring namespace, or the file-level `namespace`
    /// for top-level statements (Zend/tests/namespaces).
    pub fn caller_ns(&self) -> String {
        self.stack
            .last()
            .map(|f| f.ns.clone())
            .unwrap_or_else(|| self.globals.ns.clone())
    }

    pub fn caller_scope_name(&self) -> Option<String> {
        self.stack.last().and_then(|f| {
            f.decl_class
                .as_ref()
                .or(f.scope_class.as_ref())
                .map(|c| c.name().to_string())
        })
    }
}
