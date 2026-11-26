#!/usr/bin/env bash
# Brings this fork up to the latest upstream deslop-please and reinstalls the
# binary with `--mcp` support.
#
#   main        mirrors upstream/main exactly; nothing of ours lives there.
#   mcp-server  main plus our MCP commit(s); the only branch to install from.
#
# Stops at the first failure. On a rebase conflict: resolve it, run
# `git rebase --continue`, then run this script again.
#
# The whole body is one function, so bash has read all of it before
# `git checkout main` removes this file from the working tree.

main() {
    set -euo pipefail
    cd "$(git rev-parse --show-toplevel)"

    if [[ -n "$(git status --porcelain --untracked-files=no)" ]]; then
        echo "sync-upstream: commit or stash your changes first" >&2
        exit 1
    fi
    if [[ -d "$(git rev-parse --git-path rebase-merge)" || -d "$(git rev-parse --git-path rebase-apply)" ]]; then
        echo "sync-upstream: a rebase is in progress; finish it first" >&2
        exit 1
    fi

    echo "==> main <- upstream/main"
    git fetch upstream --tags
    git fetch origin
    git checkout main
    git merge --ff-only upstream/main
    git push origin main --tags

    echo "==> rebase mcp-server onto main"
    git checkout mcp-server
    git rebase main

    echo "==> test"
    cargo test --locked

    install_and_verify

    echo "==> push mcp-server"
    git push --force-with-lease origin mcp-server

    echo "sync-upstream: done; restart Claude Code so the plugin reconnects"
}

install_and_verify() {
    local branch
    branch="$(git branch --show-current)"
    if [[ "$branch" != "mcp-server" ]]; then
        echo "sync-upstream: refusing to install from '$branch'; only mcp-server has --mcp" >&2
        exit 1
    fi

    echo "==> install"
    cargo install --path . --locked --force

    echo "==> verify --mcp"
    local reply
    reply="$(printf '%s\n' \
        '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"sync-upstream","version":"0"}}}' \
        | deslop-please --mcp .)"
    if [[ "$reply" != *'"result"'* ]]; then
        echo "sync-upstream: installed deslop-please did not answer an MCP initialize" >&2
        exit 1
    fi
    deslop-please --version
}

main "$@"
