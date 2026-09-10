#!/usr/bin/env node
import { runMcpStdio } from '../lib/mcp-stdio.mjs';
import { handleStructureMcpRequest } from '../lib/structure-mcp.mjs';

runMcpStdio({ handleRequest: handleStructureMcpRequest });
