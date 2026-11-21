"""Package one native EpocNote build, or verify a package on a fresh runner."""

import argparse
import hashlib
import json
import os
from pathlib import Path
import queue
import shutil
import subprocess
import tarfile
import tempfile
import threading
import zipfile


def sha256(path):
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def package(args):
    destination = Path(args.output)
    destination.mkdir(parents=True, exist_ok=True)
    executable = Path(args.binary)
    files = [executable, Path(args.engine), Path("README.md")]
    with tempfile.TemporaryDirectory() as staging:
        build_info = Path(staging) / "BUILD.txt"
        build_info.write_text(
            f"Platform: {args.platform}\n"
            f"Oldest tested OS: {args.minimum_os}\n"
            f"Commit: {args.commit}\n",
            encoding="utf-8",
        )
        files.append(build_info)
        if executable.suffix == ".exe":
            archive = destination / f"epocnote-{args.platform}.zip"
            with zipfile.ZipFile(archive, "w", zipfile.ZIP_DEFLATED) as output:
                for path in files:
                    output.write(path, path.name)
        else:
            archive = destination / f"epocnote-{args.platform}.tar.gz"
            with tarfile.open(archive, "w:gz") as output:
                for path in files:
                    output.add(path, arcname=path.name)
    archive.with_name(f"{archive.name}.sha256").write_text(
        f"{sha256(archive)}  {archive.name}\n", encoding="utf-8"
    )


def unpack(archive, destination):
    expected, filename = archive.with_name(f"{archive.name}.sha256").read_text().split()
    if filename != archive.name or expected != sha256(archive):
        raise RuntimeError(f"checksum mismatch for {archive.name}")
    if archive.suffix == ".zip":
        with zipfile.ZipFile(archive) as source:
            source.extractall(destination)
        return destination / "epocnote.exe"
    with tarfile.open(archive) as source:
        source.extractall(destination, filter="data")
    return destination / "epocnote"


def exchange(executable, project, env, operations):
    metadata = {
        "io.modelcontextprotocol/protocolVersion": "2026-07-28",
        "io.modelcontextprotocol/clientInfo": {"name": "package-smoke", "version": "1"},
        "io.modelcontextprotocol/clientCapabilities": {},
    }
    with subprocess.Popen(
        [executable, "serve", "--root", project],
        stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=None,
        env=env, cwd=project, encoding="utf-8",
    ) as process:
        replies = queue.Queue()

        def read_replies():
            for line in process.stdout:
                replies.put(line)

        reader = threading.Thread(target=read_replies, daemon=True)
        reader.start()

        def request(number, method, params):
            message = {
                "jsonrpc": "2.0", "id": number, "method": method,
                "params": {"_meta": metadata, **params}
            }
            process.stdin.write(json.dumps(message) + "\n")
            process.stdin.flush()
            reply = json.loads(replies.get(timeout=15))
            assert reply["id"] == number and "error" not in reply, reply
            result = reply["result"]
            if "structuredContent" in result:
                assert not result["isError"], result
                assert json.loads(result["content"][0]["text"]) == result["structuredContent"]
                return result["structuredContent"]
            return result

        try:
            tools = request(1, "tools/list", {})
            assert {tool["name"] for tool in tools["tools"]} == {
                "epocnote_search", "epocnote_note"
            }
            return [
                request(number, "tools/call", {"name": name, "arguments": arguments})
                for number, (name, arguments) in enumerate(operations, 2)
            ]
        finally:
            process.stdin.close()
            try:
                assert process.wait(timeout=15) == 0
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait()
                raise
            reader.join(timeout=5)


def verify(args):
    artifacts = Path(args.artifacts)
    archives = sorted([*artifacts.glob("*.zip"), *artifacts.glob("*.tar.gz")])
    if len(archives) != 1:
        raise RuntimeError("expected exactly one platform package")
    with tempfile.TemporaryDirectory(prefix="epocnote package ") as temp:
        root = Path(temp)
        install = root / "current installation"
        install.mkdir()
        executable = unpack(archives[0], install)
        project = root / "project with spaces"
        (project / "src").mkdir(parents=True)
        # The smoke process resolves no tools from PATH, including Rust and SQLite.
        env = dict(os.environ, PATH="")
        subprocess.run([executable, "init", "--root", project, "--target", "codex"], env=env, check=True)
        assert (project / ".agents/skills/epocnote/SKILL.md").is_file()
        arguments = {"query": "roundtrip", "path_filter": "src"}
        note, search = exchange(executable, project, env, [
            ("epocnote_note", {"title": "Package smoke", "body": "roundtrip marker", "impacts": ["src"]}),
            ("epocnote_search", arguments),
        ])
        assert note["saved"] and note["indexed"]
        assert search["results"][0]["path"] == note["path"]
        path = project / note["path"]
        markdown = path.read_bytes()
        architecture = (project / ".context/architecture.md").read_bytes()

        replacement = root / "replacement installation"
        replacement.mkdir()
        shutil.copy2(unpack(archives[0], replacement), executable)
        subprocess.run([executable, "init", "--root", project], env=env, check=True)
        assert path.read_bytes() == markdown
        assert (project / ".context/architecture.md").read_bytes() == architecture
        search, = exchange(executable, project, env, [("epocnote_search", arguments)])
        assert search["results"][0]["path"] == note["path"]
    print(f"Verified {archives[0].name}, checksum, stdio round trip, and Markdown preservation")


def main():
    parser = argparse.ArgumentParser()
    commands = parser.add_subparsers(dest="command", required=True)
    build = commands.add_parser("build")
    for option in ("binary", "engine", "platform", "minimum-os", "commit", "output"):
        build.add_argument(f"--{option}", required=True)
    smoke = commands.add_parser("verify")
    smoke.add_argument("artifacts")
    args = parser.parse_args()
    if args.command == "build":
        package(args)
    else:
        verify(args)


if __name__ == "__main__":
    main()
