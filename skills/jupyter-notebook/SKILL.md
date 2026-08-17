---
name: jupyter-notebook
description: "Execute Jupyter notebooks (.ipynb) using nbconvert, capturing outputs and updating the notebook with execution results."
version: 1.0.0
license: Apache-2.0
when_to_use: Need to run a Jupyter notebook end-to-end and capture cell outputs, errors, and execution results. Useful for data science workflows, verifying notebooks run correctly, or executing notebooks as part of an automated pipeline.
caveats:
  exec:
    only:
      - "jupyter"
      - "nbconvert"
  fs_read:
    only:
      - "*.ipynb"
  fs_write:
    only:
      - "*.ipynb"
  net: { only: [] }
  max_calls: { at_most: 10 }
---

# Jupyter Notebook Execution Skill

This skill provides the ability to execute Jupyter notebooks (`.ipynb` files)
using nbconvert, capturing all cell outputs, errors, and execution metadata.
The notebook is updated in-place with execution results.

> **Port note.** This skill ships with **gilamonster-agent** (ported from
> newt-agent PR #1730). The tool lives in `gilamonster_agent::gila_jupyter` and
> is surfaced as the `gila jupyter execute` subcommand (compiled in with
> `--features jupyter`).

## Prerequisites

- **Jupyter** must be installed and available in PATH (`jupyter` command)
- **nbconvert** is typically included with Jupyter installation
- A Python kernel (typically `python3`) must be available
- gilamonster-agent built with the `jupyter` cargo feature

## Core Tool

`gila jupyter execute` wraps `jupyter nbconvert --execute --inplace`. The Rust
entry point is `gilamonster_agent::gila_jupyter::execute_notebook`.

## Parameters

| Parameter | CLI flag | Type | Default | Description |
|-----------|----------|------|---------|-------------|
| `notebook_path` | (positional) | string | **required** | Path to the notebook file (.ipynb) |
| `working_dir` | `--working-dir` | string | notebook's parent directory | Working directory for execution |
| `timeout_seconds` | `--timeout` | integer | 300 | Per-cell nbconvert `ExecutePreprocessor.timeout` |
| `save_outputs` | `--no-save-outputs` | boolean | true (saved) | Whether to save executed notebook with outputs |
| `kernel_name` | `--kernel` | string | "python3" | Kernel name to use for execution |

## Returns

A `JupyterExecuteResult` (printed as JSON by the CLI) containing:
- `success` (bool): Whether execution succeeded
- `notebook_path` (string): Path to the executed notebook
- `cells_executed` (int): Number of code cells executed (markdown/raw excluded)
- `cells_failed` (int): Number of code cells that failed
- `execution_time_seconds` (float): Total execution time
- `error` (string, optional): Error message if execution failed
- `cell_outputs` (list of `CellOutputSummary`): Per-cell execution summary

## CellOutputSummary

Each cell output summary contains:
- `cell_index` (int): Index of the cell
- `cell_type` (string): "code", "markdown", or "raw"
- `success` (bool): Whether the cell executed successfully
- `output_count` (int): Number of outputs produced
- `error` (string, optional): Error message if cell failed

## Example Usage

```bash
# Execute a notebook with a 60s per-cell timeout, saving outputs in place
gila jupyter execute analysis.ipynb --timeout 60
```

From Rust:

```rust
use gilamonster_agent::gila_jupyter::{execute_notebook, JupyterExecuteParams};

let result = execute_notebook(JupyterExecuteParams {
    notebook_path: "analysis.ipynb".to_string(),
    timeout_seconds: Some(60),
    save_outputs: Some(true),
    ..Default::default() // (fields are not Default; construct explicitly)
})?;

if result.success {
    println!("Executed {} cells in {:.1}s",
             result.cells_executed, result.execution_time_seconds);
}
```

## Workflow

1. **Prepare** the notebook file (`.ipynb`) with code cells ready to execute
2. **Call** `gila jupyter execute` with the notebook path and optional flags
3. **Check** the result for success/failure and inspect cell outputs
4. **The notebook file is updated in-place** with execution outputs unless
   `--no-save-outputs` is set

## Tips

- For long-running notebooks, increase `--timeout` (default 300s/5min)
- Use `--working-dir` to control the execution context (e.g., for relative
  paths in the notebook)
- The `--kernel` should match an installed Jupyter kernel (list with
  `jupyter kernelspec list`)
- With `--no-save-outputs`, the notebook is executed but not updated with
  outputs

## Error Handling

Common errors:
- **Notebook not found**: Check the `notebook_path` is correct
- **Jupyter not installed**: Install with `pip install jupyter`
- **Kernel not found**: Install the required kernel or use a different `--kernel`
- **Timeout**: Increase `--timeout` or optimize slow cells
- **Cell errors**: Check `cell_outputs` for specific cell failures with tracebacks