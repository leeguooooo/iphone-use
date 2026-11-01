#!/usr/bin/env python3
"""Reject oracle changes outside the specifically reviewed seed repair."""
import ast
import importlib.util
from pathlib import Path
import unittest

HERE = Path(__file__).resolve().parent
spec = importlib.util.spec_from_file_location('guard', HERE / 'check-oracle-recovery.py')
guard = importlib.util.module_from_spec(spec)
spec.loader.exec_module(guard)
SOURCE = (HERE / 'test-wait-simulator.py').read_text()
OLD_CHECK = ast.parse("for row in fixture.EXPECTED_ROWS:assert any(item.get('identifier')==row['identifier'] and item.get('label')==row['label'] for item in seed.get('elements',[])),'missing declared App control'").body[0]


def original():
    tree = ast.parse(SOURCE)
    tree.body = [n for n in tree.body if not (isinstance(n, ast.FunctionDef) and n.name == 'seed_errors')]

    class Restore(ast.NodeTransformer):
        def visit_Assert(self, node):
            if isinstance(node.test, ast.UnaryOp) and isinstance(node.test.operand, ast.Call) and isinstance(node.test.operand.func, ast.Name) and node.test.operand.func.id == 'seed_errors':
                return OLD_CHECK
            return node

    return ast.unparse(ast.fix_missing_locations(Restore().visit(tree)))


class Recovery(unittest.TestCase):
    def test_canonical_hash_ignores_empty_optional_ast_fields(self):
        node = ast.parse('def probe(): return 1').body[0]
        expected = guard.canonical(node)
        if hasattr(node, 'type_params'):
            del node.type_params
        self.assertEqual(guard.canonical(node), expected)

    def test_only_reviewed_inventory_check_is_compatible(self):
        self.assertEqual(guard.without_seed_check(original(), False), guard.without_seed_check(SOURCE, True))

    def test_helper_cannot_be_weakened_or_fed_synthetic_rows(self):
        weakened = SOURCE.replace("if not isinstance(rows,list)", "if False and not isinstance(rows,list)", 1)
        with self.assertRaises(AssertionError):
            guard.without_seed_check(weakened, True)
        spoofed = SOURCE.replace('assert not seed_errors(seed),seed_errors(seed)', 'assert not seed_errors({"elements":fixture.EXPECTED_ROWS}),seed_errors(seed)')
        with self.assertRaises(AssertionError):
            guard.without_seed_check(spoofed, True)

    def test_seed_assert_cannot_move_to_a_dead_branch(self):
        moved = SOURCE.replace('  assert not seed_errors(seed),seed_errors(seed)',
                               '  if False:\n   assert not seed_errors(seed),seed_errors(seed)')
        self.assertNotEqual(guard.without_seed_check(original(), False), guard.without_seed_check(moved, True))

    def test_other_wait_behavior_changes_remain_visible(self):
        changed = SOURCE.replace("'applied_actions'),0", "'applied_actions'),1", 1)
        self.assertNotEqual(changed, SOURCE)
        self.assertNotEqual(guard.without_seed_check(original(), False), guard.without_seed_check(changed, True))

    def test_recipe_addition_does_not_allow_count_or_driver_changes(self):
        old = "PROBES={'wait':('test-wait-simulator.py',22)}\nx=1"
        new = "PROBES={'wait':('test-wait-simulator.py',22),'state':('test-state-simulator.py',7)}\nx=1"
        self.assertEqual(guard.without_recipe(old), guard.without_recipe(new))
        with self.assertRaises(AssertionError):
            guard.without_recipe(new.replace(',22)', ',1)'))
        self.assertNotEqual(guard.without_recipe(old), guard.without_recipe(new.replace('x=1', 'x=2')))


if __name__ == '__main__':
    unittest.main()
