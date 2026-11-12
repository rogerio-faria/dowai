from dowai_plugins.email import send

send(to="ops@example.org", subject="Alerta")
send(to=["ops@example.org"])
