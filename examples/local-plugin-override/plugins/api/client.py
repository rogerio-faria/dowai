"""Plugin global `api` do exemplo local-plugin-override (ENT-085). Não usa rede."""


def call(**kwargs):
    return {"origin": "global", "status": None, "body": None}
