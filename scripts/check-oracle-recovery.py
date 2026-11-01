#!/usr/bin/env python3
"""Allow a wait seed-check repair while keeping product/fixture semantics pinned."""
import ast
import hashlib
import json

if not __debug__:
    raise RuntimeError("assertions required")
from pathlib import Path
import subprocess


def canonical(node):
    def shape(value):
        if isinstance(value, ast.AST):
            fields = {name: shape(item) for name, item in ast.iter_fields(value)
                      if item is not None and not (isinstance(item, list) and not item)}
            return [type(value).__name__, fields]
        if isinstance(value, list):
            return [shape(item) for item in value]
        return value
    return json.dumps(shape(node), sort_keys=True, ensure_ascii=True, separators=(',', ':'))


def blob(repo, ref, path):
    return subprocess.check_output(['git', '-C', str(repo), 'show', ref + ':' + path])


def without_recipe(source):
    tree = ast.parse(source)
    recipes = [node for node in tree.body if isinstance(node, ast.Assign)
               and any(isinstance(t, ast.Name) and t.id == 'PROBES' for t in node.targets)]
    assert len(recipes) == 1
    recipe = ast.literal_eval(recipes[0].value)
    assert recipe['wait'] == ('test-wait-simulator.py', 22)
    tree.body.remove(recipes[0])
    return canonical(tree)


def without_seed_check(source, repaired):
    tree = ast.parse(source)
    helpers = [node for node in tree.body if isinstance(node, ast.FunctionDef) and node.name == 'seed_errors']
    assert len(helpers) == int(repaired)
    for helper in helpers:
        assert hashlib.sha256(canonical(helper).encode()).hexdigest() == '223b7b058ae74e481e0f53e72c6767480859fd2c6bed36273c1d1c6c6dbc3377', "unreviewed seed helper"
        tree.body.remove(helper)

    class Strip(ast.NodeTransformer):
        removed = 0

        def visit_For(self, node):
            if (not repaired and isinstance(node.iter, ast.Attribute) and node.iter.attr == 'EXPECTED_ROWS'
                    and isinstance(node.iter.value, ast.Name) and node.iter.value.id == 'fixture'):
                assert hashlib.sha256(canonical(node).encode()).hexdigest() == 'd60fa5d2249150d8a5f63707596da4d4f2cb54ffdee892c9474dbc9ee1d8b851', "unexpected original seed check"
                self.removed += 1
                return ast.Expr(value=ast.Constant(value="APPROVED_SEED_INVENTORY_CHECK"))
            return self.generic_visit(node)

        def visit_Assert(self, node):
            approved = ast.parse("assert not seed_errors(seed), seed_errors(seed)").body[0]
            if repaired and canonical(node) == canonical(approved):
                self.removed += 1
                return ast.Expr(value=ast.Constant(value="APPROVED_SEED_INVENTORY_CHECK"))
            return node

    strip = Strip()
    tree = strip.visit(tree)
    assert strip.removed == 1, 'exactly one seed inventory check may change'
    return canonical(tree)


def verify(repo, product, oracle):
    stable = ['scripts/test-selector-simulator.py', 'scripts/mcp_probe_peer.py',
              'scripts/test-drag-simulator.py', 'scripts/simulator_ui_readiness.py',
              'scripts/fixtures/element-selector/main.swift']
    allowed = {
        '.github/workflows/pr-checks.yml', '.github/workflows/candidate-native-oracle.yml',
        '.github/workflows/simulator-smoke.yml', 'crates/server/src/runner.rs',
        'runner/IPhoneUseRunner/IPhoneUseRunnerUITests/IPURBridge.m',
        'runner/IPhoneUseRunner/IPhoneUseRunnerUITests/RunnerElements.swift',
        'runner/IPhoneUseRunner/IPhoneUseRunnerUITests/RunnerRoutes.swift',
        'scripts/test-native-state-availability.py', 'scripts/run-collect-simulator.py',
        'scripts/check-oracle-recovery.py', 'scripts/test-oracle-recovery.py',
        'scripts/fixtures/state-availability/main.swift', 'scripts/test-state-simulator.py',
        'scripts/test-state-simulator-verdict.py', 'scripts/test-wait-simulator.py',
        'scripts/test-wait-simulator-verdict.py',
    }
    product = subprocess.check_output(['git', '-C', str(repo), 'rev-parse', '--verify', product + '^{commit}'], text=True).strip()
    oracle = subprocess.check_output(['git', '-C', str(repo), 'rev-parse', '--verify', oracle + '^{commit}'], text=True).strip()
    changed = subprocess.check_output(['git', '-C', str(repo), 'diff', '--no-renames', '--name-only', product, oracle], text=True).splitlines()
    assert set(changed).issubset(allowed), 'unreviewed changed dependency'
    product_tree = subprocess.check_output(['git', '-C', str(repo), 'ls-tree', '-r', product, 'runner/gesture-fixture'])
    oracle_tree = subprocess.check_output(['git', '-C', str(repo), 'ls-tree', '-r', oracle, 'runner/gesture-fixture'])
    assert product_tree and product_tree == oracle_tree, 'fixture project tree changed or empty'
    project = subprocess.check_output(['git', '-C', str(repo), 'ls-tree', '-r', '--name-only',
                                       product, 'runner/gesture-fixture'], text=True).splitlines()
    for path in stable + project:
        assert blob(repo, product, path) == blob(repo, oracle, path), 'changed oracle dependency: ' + path
    assert without_recipe(blob(repo, product, 'scripts/run-collect-simulator.py')) == without_recipe(
        blob(repo, oracle, 'scripts/run-collect-simulator.py')), 'driver changed beyond recipe additions'
    assert without_seed_check(blob(repo, product, 'scripts/test-wait-simulator.py'), False) == without_seed_check(
        blob(repo, oracle, 'scripts/test-wait-simulator.py'), True), 'wait cases/actions/verdict changed beyond seed check'
    return {'compatible': True, 'product_commit': product, 'oracle_commit': oracle,
            'unchanged_dependencies': stable + project, 'whole_diff_allowlisted': changed,
            'invoked_wait_test_change': 'seed inventory normalization and uniqueness only; cases/actions unchanged'}


if __name__ == '__main__':
    import json
    import sys
    if not __debug__:
        raise RuntimeError('assertions required')
    print(json.dumps(verify(Path(sys.argv[1]), sys.argv[2], sys.argv[3])))
