#!/usr/bin/env python3
"""Benchmark: PyCG vs sem vs stack-graphs on dependency resolution.

Runs all three tools on a synthetic Python project with known dependency edges,
normalizes outputs, and compares against ground truth.

Usage:
    python3 benchmarks/dependency-accuracy/run_benchmark.py
"""

import ast
import json
import os
import re
import shutil
import subprocess
import sys
from pathlib import Path

BENCHMARK_DIR = Path(__file__).parent
PROJECT_DIR = BENCHMARK_DIR / "project"
GROUND_TRUTH_PATH = BENCHMARK_DIR / "ground_truth.json"

SEM_BINARY = Path.home() / "sem" / "crates" / "target" / "release" / "sem"
PYCG_PYTHON = Path("/tmp/pycg_env311/bin/python3.11")
STACK_GRAPHS_BIN = shutil.which("tree-sitter-stack-graphs-python") or str(
    Path.home() / ".cargo" / "bin" / "tree-sitter-stack-graphs-python"
)

PROJECT_FILES = [
    "core.py", "models.py", "utils.py", "io_handler.py",
    "processors.py", "api.py", "noise.py", "compat.py",
]


def check_tools():
    """Verify all tools are available."""
    errors = []
    if not SEM_BINARY.exists():
        errors.append(f"sem binary not found at {SEM_BINARY}")
    if not PYCG_PYTHON.exists():
        errors.append(
            f"PyCG Python env not found at {PYCG_PYTHON}\n"
            "  Fix: python3.11 -m venv /tmp/pycg_env311 && "
            "/tmp/pycg_env311/bin/pip install pycg"
        )
    else:
        result = subprocess.run(
            [str(PYCG_PYTHON), "-c", "from pycg.pycg import CallGraphGenerator"],
            capture_output=True, text=True,
        )
        if result.returncode != 0:
            errors.append(f"PyCG import failed: {result.stderr.strip()}")
    if not Path(STACK_GRAPHS_BIN).exists():
        errors.append(
            f"stack-graphs binary not found at {STACK_GRAPHS_BIN}\n"
            "  Fix: cargo install tree-sitter-stack-graphs-python --features cli"
        )
    if errors:
        for e in errors:
            print(f"ERROR: {e}")
        sys.exit(1)


def run_pycg():
    """Run PyCG and return normalized edges."""
    entry_points = [str(PROJECT_DIR / f) for f in PROJECT_FILES]
    cmd = [
        str(PYCG_PYTHON), "-m", "pycg",
        "--package", str(PROJECT_DIR),
    ] + entry_points

    result = subprocess.run(cmd, capture_output=True, text=True, cwd=str(BENCHMARK_DIR))
    if result.returncode != 0:
        print(f"PyCG failed: {result.stderr}")
        return set()

    raw = json.loads(result.stdout)
    return normalize_pycg(raw)


def normalize_pycg(raw):
    """Convert PyCG output to (caller, callee) tuples in module.entity format.

    PyCG outputs: {"module.func": ["module2.func2", ...], ...}
    We strip 'project.' prefix and filter to project-internal edges.
    """
    edges = set()
    for caller, callees in raw.items():
        caller_norm = normalize_pycg_name(caller)
        if caller_norm is None:
            continue
        for callee in callees:
            callee_norm = normalize_pycg_name(callee)
            if callee_norm is None:
                continue
            edges.add((caller_norm, callee_norm))
    return edges


def normalize_pycg_name(name):
    """Normalize a PyCG fully-qualified name to module.entity format.

    Examples:
        'project.core.validate' -> 'core.validate'
        'core.validate' -> 'core.validate'
        'models.UserModel.__init__' -> 'models.UserModel.__init__'
        '<builtin>.len' -> None (filtered)
        'functools.wraps' -> None (filtered)
    """
    if name.startswith("<builtin>"):
        return None
    if name.startswith("project."):
        name = name[len("project."):]
    parts = name.split(".")
    if len(parts) < 2:
        return None  # module-level, not an entity
    module = parts[0]
    if module not in {f.replace(".py", "") for f in PROJECT_FILES}:
        return None  # external module
    return name


def run_sem():
    """Run sem and return normalized edges."""
    edges = set()

    for filename in PROJECT_FILES:
        # Use relative path from BENCHMARK_DIR (sem --file needs relative paths)
        rel_filepath = os.path.join("project", filename)
        module = filename.replace(".py", "")

        # Get entities
        result = subprocess.run(
            [str(SEM_BINARY), "entities", rel_filepath, "--json"],
            capture_output=True, text=True, cwd=str(BENCHMARK_DIR),
        )
        if result.returncode != 0:
            print(f"sem entities failed for {filename}: {result.stderr}")
            continue
        entities = json.loads(result.stdout)

        # Get deps for each entity
        for entity in entities:
            name = entity["name"]
            parent_id = entity.get("parent_id")

            # Build caller name: module.Class.method or module.function
            caller = build_sem_entity_name(module, name, parent_id, rel_filepath)

            result = subprocess.run(
                [str(SEM_BINARY), "impact", name, "--file", rel_filepath,
                 "--deps", "--json"],
                capture_output=True, text=True, cwd=str(BENCHMARK_DIR),
            )
            if result.returncode != 0:
                continue
            data = json.loads(result.stdout)

            for dep in data.get("dependencies", []):
                dep_file = dep["file"]
                dep_module = Path(dep_file).stem
                dep_name = dep["name"]

                callee = f"{dep_module}.{dep_name}"

                edges.add((caller, callee))

    return edges


def build_sem_entity_name(module, name, parent_id, filepath):
    """Build module.entity name from sem entity data.

    parent_id looks like: "project/models.py::class::BaseModel"
    """
    if parent_id:
        # Extract class name from parent_id
        parts = parent_id.split("::")
        if len(parts) >= 3 and parts[1] == "class":
            class_name = parts[2]
            return f"{module}.{class_name}.{name}"
    return f"{module}.{name}"


def run_stack_graphs():
    """Run stack-graphs and return normalized edges."""
    # Step 1: Index the project (may already be indexed, that's fine)
    subprocess.run(
        [STACK_GRAPHS_BIN, "index", str(PROJECT_DIR)],
        capture_output=True, text=True, cwd=str(BENCHMARK_DIR),
    )

    # Step 2: Parse all files to build entity map and call sites
    modules = {f.replace(".py", "") for f in PROJECT_FILES}
    entity_map = {}   # (file, line) -> "module.Entity" or "module.Class.method"
    import_map = {}   # (file, line, col) -> "module.entity" (where the import points)
    call_sites = []   # (caller_name, file, line, col, callee_text)

    for filename in PROJECT_FILES:
        filepath = PROJECT_DIR / filename
        module = filename.replace(".py", "")
        source = filepath.read_text()
        tree = ast.parse(source, filename=filename)

        # Build entity line ranges and import map
        _build_entity_map(tree, module, filename, entity_map)
        _build_import_map(tree, module, filename, import_map)

        # Extract call sites
        _extract_call_sites(tree, module, filename, call_sites)

    # Step 3: For each call site, query stack-graphs and resolve
    edges = set()
    for caller, filename, line, col, callee_text in call_sites:
        rel_path = f"project/{filename}"
        defs = _query_stack_graphs(rel_path, line, col)

        for def_file, def_line, def_col in defs:
            # Check if definition points to an import binding
            import_target = import_map.get((def_file, def_line, def_col))
            if import_target:
                if import_target != caller:
                    edges.add((caller, import_target))
                continue

            # Otherwise, look up which entity is at that definition line
            def_module_file = Path(def_file).name
            callee = entity_map.get((def_module_file, def_line))
            if callee and callee != caller:
                edges.add((caller, callee))

    return edges


def _build_entity_map(tree, module, filename, entity_map):
    """Map (filename, line) -> module.entity for all function/class definitions."""
    for node in ast.walk(tree):
        if isinstance(node, ast.ClassDef):
            entity_map[(filename, node.lineno)] = f"{module}.{node.name}"
            for item in node.body:
                if isinstance(item, (ast.FunctionDef, ast.AsyncFunctionDef)):
                    entity_map[(filename, item.lineno)] = f"{module}.{node.name}.{item.name}"
        elif isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef)):
            # Only top-level functions (not methods, which are handled above)
            if not any(
                isinstance(p, ast.ClassDef)
                for p in ast.walk(tree)
                if node in ast.iter_child_nodes(p)
            ):
                entity_map[(filename, node.lineno)] = f"{module}.{node.name}"


def _build_import_map(tree, module, filename, import_map):
    """Map (filename, line, col) -> target module.entity for import bindings."""
    for node in ast.walk(tree):
        if isinstance(node, ast.ImportFrom):
            if not node.module:
                continue
            # Parse module path: "project.core" -> "core"
            mod_parts = node.module.split(".")
            if mod_parts[0] == "project" and len(mod_parts) >= 2:
                target_module = mod_parts[1]
            else:
                target_module = mod_parts[0]

            for alias in node.names:
                # The import binding is at (filename, line, col) where the name appears
                # Stack-graphs will point us here. We map to the actual target.
                import_map[(filename, node.lineno, alias.col_offset + 1)] = (
                    f"{target_module}.{alias.name}"
                )


def _extract_call_sites(tree, module, filename, call_sites):
    """Extract all call sites with their enclosing function as the caller."""
    # First pass: find all function/method defs with their line ranges
    func_ranges = []  # (name, start_line, end_line)
    for node in ast.walk(tree):
        if isinstance(node, ast.ClassDef):
            for item in node.body:
                if isinstance(item, (ast.FunctionDef, ast.AsyncFunctionDef)):
                    caller_name = f"{module}.{node.name}.{item.name}"
                    func_ranges.append((caller_name, item.lineno, item.end_lineno or 9999))
        elif isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef)):
            # Check if this is a top-level function (not inside a class)
            caller_name = f"{module}.{node.name}"
            func_ranges.append((caller_name, node.lineno, node.end_lineno or 9999))

    # Deduplicate: methods appear both as top-level FunctionDef walks and inside ClassDef
    seen = set()
    deduped = []
    for name, start, end in func_ranges:
        if name not in seen:
            seen.add(name)
            deduped.append((name, start, end))
    func_ranges = deduped

    def find_caller(line):
        for name, start, end in func_ranges:
            if start <= line <= end:
                return name
        return None

    # Second pass: find all call expressions and reference sites
    for node in ast.walk(tree):
        # Regular function/method calls
        if isinstance(node, ast.Call):
            func = node.func
            if isinstance(func, ast.Name):
                # Simple call: validate(x)
                caller = find_caller(node.lineno)
                if caller:
                    # 1-based col for stack-graphs
                    call_sites.append((caller, filename, node.lineno, func.col_offset + 1, func.id))
            elif isinstance(func, ast.Attribute):
                # Method call: super().serialize(), obj.method()
                caller = find_caller(node.lineno)
                if caller:
                    call_sites.append((
                        caller, filename, node.lineno,
                        func.end_col_offset - len(func.attr) + 1,
                        func.attr,
                    ))

        # Decorator references
        elif isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef)):
            for dec in node.decorator_list:
                if isinstance(dec, ast.Name):
                    # The decorated function is the "caller" of the decorator
                    # Find the enclosing scope
                    caller = f"{module}.{node.name}"
                    call_sites.append((caller, filename, dec.lineno, dec.col_offset + 1, dec.id))

        # Class inheritance: class Foo(Bar)
        elif isinstance(node, ast.ClassDef):
            for base in node.bases:
                if isinstance(base, ast.Name):
                    caller = f"{module}.{node.name}"
                    call_sites.append((caller, filename, base.lineno, base.col_offset + 1, base.id))


def _query_stack_graphs(rel_path, line, col):
    """Query stack-graphs for the definition of a reference at file:line:col.

    Returns list of (def_file, def_line, def_col) tuples.
    """
    pos = f"{rel_path}:{line}:{col}"
    result = subprocess.run(
        [STACK_GRAPHS_BIN, "query", "definition", pos],
        capture_output=True, text=True, cwd=str(BENCHMARK_DIR),
    )
    output = result.stdout + result.stderr

    if "no references at location" in output:
        return []

    # Parse "has definition" blocks
    # Format:
    #   /abs/path/to/file.py:LINE:COL:
    #   LINE | source code
    defs = []
    lines = output.split("\n")
    i = 0
    while i < len(lines):
        line_text = lines[i].strip()
        if line_text == "has definition" or line_text.startswith("has ") and "definition" in line_text:
            # Look ahead for definition location lines
            i += 1
            while i < len(lines):
                dl = lines[i].strip()
                # Match: /path/to/file.py:42:5:
                m = re.match(r"(.+?):(\d+):(\d+):$", dl)
                if m:
                    def_file = m.group(1)
                    def_line = int(m.group(2))
                    def_col = int(m.group(3))
                    # Normalize to relative filename
                    def_file = Path(def_file).name
                    defs.append((def_file, def_line, def_col))
                    i += 1
                    # Skip the source line display
                    if i < len(lines):
                        i += 1
                    continue
                break
        else:
            i += 1

    # Deduplicate (stack-graphs sometimes returns same def twice)
    return list(set(defs))


def load_ground_truth():
    """Load ground truth edges."""
    with open(GROUND_TRUTH_PATH) as f:
        data = json.load(f)

    edges = set()
    categories = {}
    for edge in data["edges"]:
        key = (edge["caller"], edge["callee"])
        edges.add(key)
        categories[key] = edge["category"]

    return edges, categories


def score(found_edges, truth_edges):
    """Compute precision, recall, F1."""
    tp = found_edges & truth_edges
    fp = found_edges - truth_edges
    fn = truth_edges - found_edges

    precision = len(tp) / len(found_edges) if found_edges else 0
    recall = len(tp) / len(truth_edges) if truth_edges else 0
    f1 = 2 * precision * recall / (precision + recall) if (precision + recall) else 0

    return {
        "tp": len(tp), "fp": len(fp), "fn": len(fn),
        "precision": precision, "recall": recall, "f1": f1,
        "tp_edges": sorted(tp),
        "fp_edges": sorted(fp),
        "fn_edges": sorted(fn),
    }


def category_breakdown(found_edges, truth_edges, categories):
    """Score per category."""
    cats = sorted(set(categories.values()))
    rows = []
    for cat in cats:
        cat_truth = {e for e in truth_edges if categories.get(e) == cat}
        cat_tp = found_edges & cat_truth
        cat_fn = cat_truth - found_edges
        total = len(cat_truth)
        found = len(cat_tp)
        rows.append((cat, found, total))
    return rows


def pycg_normalize_constructors(edges, truth_edges):
    """Normalize PyCG constructor representation.

    PyCG uses Class.__init__ for constructor calls (e.g. UserModel()).
    If ground truth uses Class (not .__init__), replace to match.
    Keep .__init__ form for super() edges where ground truth uses .__init__.
    """
    result = set()
    for caller, callee in edges:
        if callee.endswith(".__init__"):
            base = callee.rsplit(".__init__", 1)[0]
            # If ground truth has the Class form, use that
            if (caller, base) in truth_edges:
                result.add((caller, base))
            else:
                result.add((caller, callee))
        else:
            result.add((caller, callee))
    return result


def print_table(headers, rows, col_widths=None):
    """Print a simple table."""
    if col_widths is None:
        col_widths = [max(len(str(r[i])) for r in [headers] + rows) for i in range(len(headers))]
    fmt = "  ".join(f"{{:<{w}}}" for w in col_widths)
    print(fmt.format(*headers))
    print(fmt.format(*["-" * w for w in col_widths]))
    for row in rows:
        print(fmt.format(*row))


def main():
    check_tools()
    truth_edges, categories = load_ground_truth()

    print(f"Ground truth: {len(truth_edges)} edges across {len(set(categories.values()))} categories\n")

    # Run tools
    print("Running PyCG...")
    pycg_edges = run_pycg()
    pycg_edges = pycg_normalize_constructors(pycg_edges, truth_edges)

    print("Running sem...")
    sem_edges = run_sem()

    print("Running stack-graphs...")
    sg_edges = run_stack_graphs()
    print()

    # Score
    pycg_scores = score(pycg_edges, truth_edges)
    sem_scores = score(sem_edges, truth_edges)
    sg_scores = score(sg_edges, truth_edges)

    # Summary table
    print("=" * 70)
    print("RESULTS")
    print("=" * 70)
    print()
    print_table(
        ["Tool", "Precision", "Recall", "F1", "TP", "FP", "FN"],
        [
            ["PyCG", f"{pycg_scores['precision']:.1%}", f"{pycg_scores['recall']:.1%}",
             f"{pycg_scores['f1']:.1%}", pycg_scores["tp"], pycg_scores["fp"], pycg_scores["fn"]],
            ["sem", f"{sem_scores['precision']:.1%}", f"{sem_scores['recall']:.1%}",
             f"{sem_scores['f1']:.1%}", sem_scores["tp"], sem_scores["fp"], sem_scores["fn"]],
            ["stack-graphs", f"{sg_scores['precision']:.1%}", f"{sg_scores['recall']:.1%}",
             f"{sg_scores['f1']:.1%}", sg_scores["tp"], sg_scores["fp"], sg_scores["fn"]],
        ],
        [13, 10, 8, 6, 4, 4, 4],
    )

    # Category breakdown
    print()
    print("PER-CATEGORY RECALL (found / total)")
    print()
    pycg_cat = category_breakdown(pycg_edges, truth_edges, categories)
    sem_cat = category_breakdown(sem_edges, truth_edges, categories)
    sg_cat = category_breakdown(sg_edges, truth_edges, categories)
    cat_rows = []
    for (cat, pf, pt), (_, sf, st), (_, gf, gt) in zip(pycg_cat, sem_cat, sg_cat):
        cat_rows.append([cat, f"{pf}/{pt}", f"{sf}/{st}", f"{gf}/{gt}"])
    print_table(
        ["Category", "PyCG", "sem", "stack-graphs"],
        cat_rows,
        [16, 8, 8, 13],
    )

    # Detail: false positives
    print()
    for tool_name, tool_scores in [("PyCG", pycg_scores), ("sem", sem_scores), ("stack-graphs", sg_scores)]:
        print(f"{tool_name} false positives ({tool_scores['fp']}):")
        for caller, callee in sorted(tool_scores["fp_edges"]):
            print(f"  {caller} -> {callee}")
        print()

    # Detail: false negatives
    for tool_name, tool_scores in [("PyCG", pycg_scores), ("sem", sem_scores), ("stack-graphs", sg_scores)]:
        print(f"{tool_name} false negatives ({tool_scores['fn']}):")
        for caller, callee in sorted(tool_scores["fn_edges"]):
            cat = categories.get((caller, callee), "?")
            print(f"  {caller} -> {callee}  [{cat}]")
        print()

    # Write results JSON
    results = {
        "ground_truth_count": len(truth_edges),
        "pycg": {
            "edges_found": len(pycg_edges),
            **{k: v for k, v in pycg_scores.items() if k not in ("tp_edges", "fp_edges", "fn_edges")},
            "false_positives": [list(e) for e in sorted(pycg_scores["fp_edges"])],
            "false_negatives": [list(e) for e in sorted(pycg_scores["fn_edges"])],
        },
        "sem": {
            "edges_found": len(sem_edges),
            **{k: v for k, v in sem_scores.items() if k not in ("tp_edges", "fp_edges", "fn_edges")},
            "false_positives": [list(e) for e in sorted(sem_scores["fp_edges"])],
            "false_negatives": [list(e) for e in sorted(sem_scores["fn_edges"])],
        },
        "stack_graphs": {
            "edges_found": len(sg_edges),
            **{k: v for k, v in sg_scores.items() if k not in ("tp_edges", "fp_edges", "fn_edges")},
            "false_positives": [list(e) for e in sorted(sg_scores["fp_edges"])],
            "false_negatives": [list(e) for e in sorted(sg_scores["fn_edges"])],
        },
    }
    results_path = BENCHMARK_DIR / "results.json"
    with open(results_path, "w") as f:
        json.dump(results, f, indent=2)
    print(f"Full results written to {results_path}")


if __name__ == "__main__":
    main()
