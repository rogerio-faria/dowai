"""Plugin oficial `sftp` (ENT-113, plano §9.5). Código entregue; testes adiados (D-117).

Entrypoints (plugin.json):
- download: copia um arquivo remoto para o projeto, com escrita atômica.
- upload: envia um arquivo local do projeto para o servidor, com renomeação no servidor.

Regras:
- Credenciais vêm do segredo `sftp_credentials` (JSON com `username` e `password` ou
  `private_key` em PEM). Nunca são aceitas como argumento de entrada nem registradas em erro.
- Verificação de host obrigatória: `known_hosts` dentro de `base_dir`; host desconhecido ou com
  chave divergente é recusado (`DOWAI_SFTP_HOST_KEY_FAILED`). Sem agente SSH e sem chaves do
  usuário (`look_for_keys=False`, `allow_agent=False`).
- Caminhos locais são relativos a `base_dir`, sem `..`, sem symlink (mesma regra do plugin files).
- Caminhos remotos devem ser absolutos POSIX, sem `..` nem componente vazio.
- Falha durante a renomeação no servidor pode ter aplicado o efeito: sai com código 13
  (incerto), e o host não reexecuta sem verificação.
"""

import os
import re
import json
import secrets as _random
import tempfile

DEFAULT_PORT = 22
UNCERTAIN_EXIT = 13


class SftpError(Exception):
    """Falha do plugin com código estável."""

    def __init__(self, code, message, uncertain=False):
        super().__init__(message)
        self.code = code
        if uncertain:
            self.dowai_exit_code = UNCERTAIN_EXIT


def _text(value, field):
    if not isinstance(value, str) or not value.strip() or "\x00" in value:
        raise SftpError("DOWAI_SFTP_INPUT_INVALID", f"{field} deve ser texto não vazio")
    return value


def _port(value):
    if value is None:
        return DEFAULT_PORT
    if not isinstance(value, int) or isinstance(value, bool) or not 1 <= value <= 65535:
        raise SftpError("DOWAI_SFTP_INPUT_INVALID", "port deve ser inteiro de 1 a 65535")
    return value


def _if_exists(value):
    if value not in ("replace", "fail"):
        raise SftpError("DOWAI_SFTP_INPUT_INVALID", "if_exists deve ser replace ou fail")
    return value


def _remote_path(value, field):
    path = _text(value, field)
    if not path.startswith("/"):
        raise SftpError("DOWAI_SFTP_PATH_DENIED", f"{field} deve ser absoluto no servidor")
    parts = path.split("/")[1:]
    if any(part in ("", ".", "..") for part in parts) or "\\" in path:
        raise SftpError("DOWAI_SFTP_PATH_DENIED", f"{field} com componente inválido")
    return path


def _local_path(base_dir, rel, field):
    """Resolve `rel` dentro de `base_dir` sem seguir symlinks (mesma regra de plugins/files)."""
    base_dir = _text(base_dir, "base_dir")
    rel = _text(rel, field)
    if os.path.isabs(rel) or re.match(r"^[A-Za-z]:", rel):
        raise SftpError("DOWAI_SFTP_PATH_DENIED", f"{field} deve ser relativo à raiz permitida")
    parts = rel.replace("\\", "/").split("/")
    if any(part in ("", ".", "..") for part in parts):
        raise SftpError("DOWAI_SFTP_PATH_DENIED", f"{field} com componente inválido")
    base = os.path.realpath(base_dir)
    if not os.path.isdir(base):
        raise SftpError("DOWAI_SFTP_PATH_DENIED", "base_dir não é um diretório")
    current = base
    for part in parts[:-1]:
        current = os.path.join(current, part)
        if os.path.islink(current):
            raise SftpError("DOWAI_SFTP_PATH_DENIED", "symlink não permitido no caminho")
        if os.path.exists(current) and not os.path.isdir(current):
            raise SftpError("DOWAI_SFTP_PATH_DENIED", "componente do caminho não é diretório")
    target = os.path.join(current, parts[-1])
    if os.path.islink(target):
        raise SftpError("DOWAI_SFTP_PATH_DENIED", "symlink não permitido no destino")
    if os.path.commonpath([base, os.path.realpath(current)]) != base:
        raise SftpError("DOWAI_SFTP_PATH_DENIED", "caminho sai da raiz permitida")
    return target


def _known_hosts(base_dir, rel):
    path = _local_path(base_dir, rel, "known_hosts")
    if not os.path.isfile(path):
        raise SftpError("DOWAI_SFTP_HOST_KEY_FAILED", "known_hosts obrigatório e não encontrado")
    return path


def _credentials(secrets):
    raw = (secrets or {}).get("sftp_credentials")
    if not raw:
        raise SftpError("DOWAI_SFTP_CREDENTIALS_MISSING", "segredo sftp_credentials não disponível")
    try:
        data = json.loads(raw)
    except (TypeError, ValueError):
        raise SftpError("DOWAI_SFTP_CREDENTIALS_MISSING", "sftp_credentials deve ser JSON") from None
    if not isinstance(data, dict) or not isinstance(data.get("username"), str):
        raise SftpError("DOWAI_SFTP_CREDENTIALS_MISSING", "sftp_credentials sem username")
    if not (data.get("password") or data.get("private_key")):
        raise SftpError("DOWAI_SFTP_CREDENTIALS_MISSING", "sftp_credentials sem password ou private_key")
    return data


def _paramiko():
    try:
        import paramiko
    except ImportError as exc:
        raise SftpError(
            "DOWAI_SFTP_DEPENDENCY_MISSING", "SFTP requer paramiko no ambiente do run"
        ) from exc
    return paramiko


def _load_key(paramiko, pem):
    import io

    try:
        return paramiko.PKey.from_private_key(io.StringIO(pem))
    except Exception:  # noqa: BLE001 - conteúdo da chave nunca sai no erro
        raise SftpError("DOWAI_SFTP_CREDENTIALS_MISSING", "private_key inválida") from None


def _connect(host, port, credentials, known_hosts_path, timeout_seconds):
    paramiko = _paramiko()
    client = paramiko.SSHClient()
    try:
        client.load_host_keys(known_hosts_path)
    except Exception:  # noqa: BLE001 - detalhes do arquivo não saem do plugin
        raise SftpError("DOWAI_SFTP_HOST_KEY_FAILED", "known_hosts inválido") from None
    client.set_missing_host_key_policy(paramiko.RejectPolicy())
    kwargs = {
        "hostname": host,
        "port": port,
        "username": credentials["username"],
        "timeout": timeout_seconds,
        "look_for_keys": False,
        "allow_agent": False,
    }
    if credentials.get("private_key"):
        kwargs["pkey"] = _load_key(paramiko, credentials["private_key"])
    else:
        kwargs["password"] = credentials["password"]
    try:
        client.connect(**kwargs)
    except paramiko.BadHostKeyException:
        client.close()
        raise SftpError("DOWAI_SFTP_HOST_KEY_FAILED", "chave do host divergente do known_hosts") from None
    except paramiko.AuthenticationException:
        client.close()
        raise SftpError("DOWAI_SFTP_AUTH_FAILED", "autenticação recusada pelo servidor") from None
    except paramiko.SSHException:
        client.close()
        raise SftpError("DOWAI_SFTP_HOST_KEY_FAILED", "falha de handshake SSH ou chave do host") from None
    except OSError:
        client.close()
        raise SftpError("DOWAI_SFTP_CONNECTION_FAILED", "não foi possível conectar ao servidor") from None
    return client


def _safe_close(client, sftp=None):
    for handle in (sftp, client):
        try:
            if handle is not None:
                handle.close()
        except Exception:  # noqa: BLE001
            pass


def download(
    base_dir,
    host,
    remote_path,
    local_path,
    known_hosts,
    port=None,
    if_exists="replace",
    timeout_seconds=30,
    secrets=None,
):
    """Baixa `remote_path` do servidor para `local_path` (relativo a base_dir), atomicamente."""
    host = _text(host, "host")
    port = _port(port)
    if_exists = _if_exists(if_exists)
    remote = _remote_path(remote_path, "remote_path")
    target = _local_path(base_dir, local_path, "local_path")
    if os.path.isdir(target):
        raise SftpError("DOWAI_SFTP_PATH_DENIED", "destino local é um diretório")
    if if_exists == "fail" and os.path.lexists(target):
        raise SftpError("DOWAI_SFTP_TARGET_EXISTS", "destino local já existe e if_exists é fail")
    known = _known_hosts(base_dir, known_hosts)
    credentials = _credentials(secrets)
    client = _connect(host, port, credentials, known, timeout_seconds)
    sftp = None
    parent = os.path.dirname(target)
    os.makedirs(parent, exist_ok=True)
    handle, tmp_path = tempfile.mkstemp(dir=parent, prefix=".dowai-sftp-", suffix=".tmp")
    os.close(handle)
    try:
        sftp = client.open_sftp()
        try:
            sftp.get(remote, tmp_path)
        except FileNotFoundError:
            raise SftpError("DOWAI_SFTP_NOT_FOUND", "arquivo remoto não encontrado") from None
        except OSError:
            raise SftpError("DOWAI_SFTP_IO_FAILED", "falha ao ler o arquivo remoto") from None
        size = os.path.getsize(tmp_path)
        with open(tmp_path, "rb") as done:
            os.fsync(done.fileno())
        os.replace(tmp_path, target)
    except BaseException:
        try:
            os.unlink(tmp_path)
        except OSError:
            pass
        raise
    finally:
        _safe_close(client, sftp)
    return {"remote_path": remote_path, "local_path": local_path, "bytes": size}


def upload(
    base_dir,
    host,
    local_path,
    remote_path,
    known_hosts,
    port=None,
    if_exists="replace",
    timeout_seconds=30,
    secrets=None,
):
    """Envia `local_path` (relativo a base_dir) para `remote_path` no servidor."""
    host = _text(host, "host")
    port = _port(port)
    if_exists = _if_exists(if_exists)
    remote = _remote_path(remote_path, "remote_path")
    source = _local_path(base_dir, local_path, "local_path")
    if not os.path.isfile(source):
        raise SftpError("DOWAI_SFTP_NOT_FOUND", "arquivo local de origem não encontrado")
    known = _known_hosts(base_dir, known_hosts)
    credentials = _credentials(secrets)
    client = _connect(host, port, credentials, known, timeout_seconds)
    sftp = None
    staged = f"{remote}.dowai-tmp-{_random.token_hex(8)}"
    try:
        sftp = client.open_sftp()
        if if_exists == "fail":
            try:
                sftp.stat(remote)
                raise SftpError("DOWAI_SFTP_TARGET_EXISTS", "destino remoto já existe e if_exists é fail")
            except FileNotFoundError:
                pass
        try:
            sftp.put(source, staged)
        except OSError:
            _try_remove(sftp, staged)
            raise SftpError("DOWAI_SFTP_IO_FAILED", "falha ao enviar o arquivo") from None
        try:
            _rename(sftp, staged, remote)
        except OSError:
            # A renomeação pode ter sido aplicada sem resposta conclusiva: estado incerto.
            raise SftpError(
                "DOWAI_SFTP_UNCERTAIN", "envio sem confirmação da renomeação no servidor", uncertain=True
            ) from None
        size = os.path.getsize(source)
    finally:
        _safe_close(client, sftp)
    return {"local_path": local_path, "remote_path": remote_path, "bytes": size}


def _rename(sftp, staged, remote):
    try:
        sftp.posix_rename(staged, remote)
    except AttributeError:
        sftp.rename(staged, remote)


def _try_remove(sftp, path):
    try:
        sftp.remove(path)
    except OSError:
        pass
