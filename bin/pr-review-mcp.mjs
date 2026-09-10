#!/usr/bin/env node
import { resolve } from 'node:path';
import { runMcpStdio } from '../lib/mcp-stdio.mjs';
import { handleReviewMcpRequest } from '../lib/review-mcp.mjs';

const root = process.env.REVIEW_MCP_ROOT;
if (root) process.chdir(resolve(root));

runMcpStdio({ handleRequest: handleReviewMcpRequest });
