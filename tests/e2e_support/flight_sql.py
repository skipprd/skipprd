"""Arrow Flight SQL probes for skipprd e2e harnesses."""

from __future__ import annotations

import os
import subprocess
import sys
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[2]
HLA_VENV = REPO_ROOT / ".skippr" / "hla-venv"


class FlightSqlError(RuntimeError):
    pass


def _varint(n: int) -> bytes:
    out = bytearray()
    while True:
        bits = n & 0x7F
        n >>= 7
        if n:
            out.append(bits | 0x80)
        else:
            out.append(bits)
            return bytes(out)


def _proto_len_delim(tag: int, value: bytes) -> bytes:
    return bytes([(tag << 3) | 2]) + _varint(len(value)) + value


def command_statement_query(sql: str) -> bytes:
    inner = _proto_len_delim(1, sql.encode())
    type_url = b"type.googleapis.com/arrow.flight.protocol.sql.CommandStatementQuery"
    return _proto_len_delim(1, type_url) + _proto_len_delim(2, inner)


def load_flight():
    try:
        import pyarrow.flight as flight

        return flight
    except ImportError:
        pass
    python = HLA_VENV / "bin" / "python"
    if not python.exists():
        HLA_VENV.parent.mkdir(parents=True, exist_ok=True)
        subprocess.check_call([sys.executable, "-m", "venv", str(HLA_VENV)])
        pip_env = os.environ.copy()
        pip_env["PIP_CONFIG_FILE"] = os.devnull
        pip_env["PIP_INDEX_URL"] = "https://pypi.org/simple"
        subprocess.check_call(
            [str(HLA_VENV / "bin" / "pip"), "install", "-q", "pyarrow"],
            env=pip_env,
        )
    for site in (HLA_VENV / "lib").glob("python*/site-packages"):
        path = str(site)
        if path not in sys.path:
            sys.path.insert(0, path)
    try:
        import pyarrow.flight as flight

        return flight
    except ImportError as err:
        raise FlightSqlError("pyarrow is required for Flight SQL") from err


def _location(addr: str) -> str:
    if "://" in addr:
        return addr
    return f"grpc://{addr}"


def query_flight_table(addr: str, sql: str, timeout: float = 10.0, token: str | None = None):
    del timeout
    flight = load_flight()
    client = flight.FlightClient(_location(addr))
    headers = []
    if token:
        headers.append((b"authorization", f"Bearer {token}".encode()))
    options = flight.FlightCallOptions(headers=headers) if headers else None
    descriptor = flight.FlightDescriptor.for_command(command_statement_query(sql))
    info = (
        client.get_flight_info(descriptor, options)
        if options
        else client.get_flight_info(descriptor)
    )
    if not info.endpoints:
        raise FlightSqlError(f"no Flight SQL endpoint from {addr}")
    reader = (
        client.do_get(info.endpoints[0].ticket, options)
        if options
        else client.do_get(info.endpoints[0].ticket)
    )
    return reader.read_all()


def query_flight_count(
    addr: str, sql: str, timeout: float = 10.0, token: str | None = None
) -> int:
    table = query_flight_table(addr, sql, timeout, token=token)
    if table.num_columns < 1 or table.num_rows < 1:
        raise FlightSqlError(f"count query on {addr} returned empty: {table.to_pydict()}")
    value = table.column(0)[0].as_py()
    if value is None:
        raise FlightSqlError(f"count query on {addr} was null")
    return int(value)
