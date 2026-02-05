import { ParserRegistry } from '../registry.js';
import { JsonParserPlugin } from './json/index.js';
import { CodeParserPlugin } from './code/index.js';
import { YamlParserPlugin } from './yaml/index.js';
import { TomlParserPlugin } from './toml/index.js';
import { CsvParserPlugin } from './csv/index.js';
import { MarkdownParserPlugin } from './markdown/index.js';
import { FallbackParserPlugin } from './fallback/index.js';

export function createDefaultRegistry(): ParserRegistry {
  const registry = new ParserRegistry();

  registry.register(new JsonParserPlugin());
  registry.register(new CodeParserPlugin());
  registry.register(new YamlParserPlugin());
  registry.register(new TomlParserPlugin());
  registry.register(new CsvParserPlugin());
  registry.register(new MarkdownParserPlugin());

  // Fallback must be last
  registry.register(new FallbackParserPlugin());

  return registry;
}
