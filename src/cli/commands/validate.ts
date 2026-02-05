import { existsSync } from 'node:fs';
import { readFile } from 'node:fs/promises';
import { resolve } from 'node:path';
import chalk from 'chalk';
import type { SemanticChange } from '../../model/change.js';
import type { DiffResult } from '../../parser/differ.js';

export interface ValidationRule {
  id: string;
  name: string;
  severity: 'error' | 'warning' | 'info';
  match: {
    changeType?: string | string[];
    entityType?: string | string[];
    filePath?: string;
  };
  condition?: string; // Simple expression: "value > 100", "content.includes('TODO')"
  message: string;
}

export interface SemConfig {
  rules?: ValidationRule[];
}

export interface ValidationResult {
  rule: ValidationRule;
  change: SemanticChange;
  message: string;
}

export async function loadConfig(repoRoot: string): Promise<SemConfig> {
  const configPath = resolve(repoRoot, '.semrc');
  const configJsonPath = resolve(repoRoot, '.semrc.json');

  let configFile: string | undefined;
  if (existsSync(configJsonPath)) {
    configFile = configJsonPath;
  } else if (existsSync(configPath)) {
    configFile = configPath;
  }

  if (!configFile) return {};

  const content = await readFile(configFile, 'utf-8');
  return JSON.parse(content) as SemConfig;
}

export function validateChanges(result: DiffResult, config: SemConfig): ValidationResult[] {
  const violations: ValidationResult[] = [];
  const rules = config.rules ?? [];

  for (const rule of rules) {
    for (const change of result.changes) {
      if (!matchesRule(change, rule)) continue;

      if (rule.condition) {
        if (!evaluateCondition(rule.condition, change)) continue;
      }

      const message = interpolateMessage(rule.message, change);
      violations.push({ rule, change, message });
    }
  }

  return violations;
}

function matchesRule(change: SemanticChange, rule: ValidationRule): boolean {
  if (rule.match.changeType) {
    const types = Array.isArray(rule.match.changeType) ? rule.match.changeType : [rule.match.changeType];
    if (!types.includes(change.changeType)) return false;
  }

  if (rule.match.entityType) {
    const types = Array.isArray(rule.match.entityType) ? rule.match.entityType : [rule.match.entityType];
    if (!types.includes(change.entityType)) return false;
  }

  if (rule.match.filePath) {
    const pattern = rule.match.filePath;
    if (pattern.includes('*')) {
      const regex = new RegExp('^' + pattern.replace(/\*/g, '.*') + '$');
      if (!regex.test(change.filePath)) return false;
    } else {
      if (!change.filePath.includes(pattern)) return false;
    }
  }

  return true;
}

function evaluateCondition(condition: string, change: SemanticChange): boolean {
  try {
    const value = change.afterContent?.trim();
    const beforeValue = change.beforeContent?.trim();
    const name = change.entityName;
    const type = change.entityType;

    // Simple safe expression evaluation
    // Supports: value > N, value < N, value == "str", content.includes("str")
    const numValue = Number(value);
    const numBefore = Number(beforeValue);

    if (condition.includes('value >')) {
      const threshold = Number(condition.split('>')[1].trim());
      return !isNaN(numValue) && numValue > threshold;
    }
    if (condition.includes('value <')) {
      const threshold = Number(condition.split('<')[1].trim());
      return !isNaN(numValue) && numValue < threshold;
    }
    if (condition.includes('value ==')) {
      const expected = condition.split('==')[1].trim().replace(/['"]/g, '');
      return value === expected;
    }
    if (condition.includes('content.includes(')) {
      const match = condition.match(/content\.includes\(['"](.+?)['"]\)/);
      if (match) {
        return (change.afterContent ?? '').includes(match[1]);
      }
    }
    if (condition.includes('increased')) {
      return !isNaN(numValue) && !isNaN(numBefore) && numValue > numBefore;
    }
    if (condition.includes('decreased')) {
      return !isNaN(numValue) && !isNaN(numBefore) && numValue < numBefore;
    }

    return false;
  } catch {
    return false;
  }
}

function interpolateMessage(message: string, change: SemanticChange): string {
  return message
    .replace(/\{name\}/g, change.entityName)
    .replace(/\{type\}/g, change.entityType)
    .replace(/\{file\}/g, change.filePath)
    .replace(/\{change\}/g, change.changeType)
    .replace(/\{value\}/g, change.afterContent?.trim() ?? '')
    .replace(/\{before\}/g, change.beforeContent?.trim() ?? '');
}

export function formatValidationResults(violations: ValidationResult[]): string {
  if (violations.length === 0) {
    return chalk.green('  All validation rules passed.');
  }

  const lines: string[] = [];
  const errors = violations.filter(v => v.rule.severity === 'error');
  const warnings = violations.filter(v => v.rule.severity === 'warning');
  const infos = violations.filter(v => v.rule.severity === 'info');

  for (const v of errors) {
    lines.push(chalk.red(`  ✗ ERROR  `) + chalk.bold(v.rule.name) + chalk.dim(` — ${v.message}`));
  }
  for (const v of warnings) {
    lines.push(chalk.yellow(`  ⚠ WARN   `) + chalk.bold(v.rule.name) + chalk.dim(` — ${v.message}`));
  }
  for (const v of infos) {
    lines.push(chalk.blue(`  ℹ INFO   `) + chalk.bold(v.rule.name) + chalk.dim(` — ${v.message}`));
  }

  lines.push('');
  if (errors.length > 0) {
    lines.push(chalk.red(`  ${errors.length} error${errors.length > 1 ? 's' : ''}, ${warnings.length} warning${warnings.length !== 1 ? 's' : ''}`));
  } else if (warnings.length > 0) {
    lines.push(chalk.yellow(`  ${warnings.length} warning${warnings.length !== 1 ? 's' : ''}`));
  }

  return lines.join('\n');
}
