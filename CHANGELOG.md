# Changelog

## Unreleased

- Add an initial Rust workspace with project initialization, validation,
  offline model planning, manual-order diagnostics, and versioned JSON output.
- Resolve implemented configuration layers in order with field origins and
  administrator ceilings; initialize workspace configuration without
  overwriting existing files.
- Add a safe offline SQL template renderer for bounded conditionals and
  session-variable defaults; SQL value interpolation remains prohibited.
- Add the preliminary typed Python SDK package; runtime integration is not
  included.
