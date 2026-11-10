"""Expose the Sanity client without logging content in command arguments."""

import json
from pathlib import Path
from typing import Annotated

import typer

from .client import SanityClient, SanityError

app = typer.Typer(no_args_is_help=True, help="Sanity content operations with audit logs")


@app.callback()
def main() -> None:
    """Run an audited Sanity operation."""


@app.command()
def call(
    operation: Annotated[str, typer.Argument(help="query, mutate, actions, or upload_asset")],
    input_file: Annotated[Path, typer.Option("--input", help="JSON arguments file")],
) -> None:
    """Read arguments from a file so content stays out of CLI analytics."""
    if operation not in {"query", "mutate", "actions", "upload_asset"}:
        raise typer.BadParameter("Unsupported Sanity operation")
    client = SanityClient()
    try:
        arguments = json.loads(input_file.read_text())
        if not isinstance(arguments, dict):
            raise typer.BadParameter("Input must be a JSON object")
        result = getattr(client, operation)(**arguments)
        print(json.dumps(result, ensure_ascii=False))
    except SanityError as error:
        typer.echo(str(error), err=True)
        raise typer.Exit(1) from None
    finally:
        client.close()
