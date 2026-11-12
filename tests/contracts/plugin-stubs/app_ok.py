from dowai_plugins import email
from dowai_plugins.email import notify, send

send(to=["ops@example.org"], subject="Alerta", message="ok", priority=1)
send(to=["ops@example.org"], subject="Alerta")
email.send(to=["ops@example.org"], subject="Alerta", options_flag=True)
