"""Public exceptions raised by the DOWAI SDK."""


class DowaiExecutionError(RuntimeError):
    """A stable, user-facing execution or runtime-boundary error."""

    def __init__(
        self,
        code: str,
        message: str,
        *,
        hint: str | None = None,
        run_id: str | None = None,
        invocation_id: str | None = None,
    ) -> None:
        if not isinstance(code, str) or not code:
            raise ValueError("code must be a non-empty string")
        if not isinstance(message, str) or not message:
            raise ValueError("message must be a non-empty string")
        super().__init__(message)
        self.code = code
        self.hint = hint
        self.run_id = run_id
        self.invocation_id = invocation_id
