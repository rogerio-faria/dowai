from pathlib import Path


def send(
    to: list[str],
    subject: str,
    message: str | None = None,
    priority: int = 0,
    attachment: Path | None = None,
    **options,
) -> dict:
    return {"sent": True}


async def notify(channel: str, retries: int = 3) -> None:
    return None
