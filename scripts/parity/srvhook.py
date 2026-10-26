#!/usr/bin/env python3
"""Run the stock rngit node with the observation hook, so every request it
receives is logged to $TRACE_LOG. Usage: srvhook.py --config DIR --rnsconfig DIR"""
import os
import runpy
import shutil
import sys

here = os.path.dirname(os.path.abspath(__file__))
exec(open(os.path.join(here, "trace_hook.py")).read())
sys.argv[0] = shutil.which("rngit")
runpy.run_path(sys.argv[0], run_name="__main__")
