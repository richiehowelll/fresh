/// <reference path="./lib/fresh.d.ts" />

import { Finder, FilterSource, defaultFuzzyFilter } from "./lib/finder.ts";

interface SymbolItem {
  name: string;
  kind: number;
  startLine: number;
  endLine: number;
}

function getKindLabel(kind: number): string {
  switch (kind) {
    case 1:
      return "file";
    case 2:
      return "mod";
    case 3:
      return "ns";
    case 4:
      return "pkg";
    case 5:
      return "class";
    case 6:
      return "method";
    case 7:
      return "prop";
    case 8:
      return "field";
    case 9:
      return "construct";
    case 10:
      return "enum";
    case 11:
      return "iface";
    case 12:
      return "fn";
    case 13:
      return "var";
    case 14:
      return "const";
    case 22:
      return "enum-mem";
    case 23:
      return "struct";
    case 24:
      return "event";
    case 25:
      return "op";
    case 26:
      return "type-param";
    default:
      return "item";
  }
}

let cachedBufferId: number | null = null;
let cachedFilePath: string = "";
let cachedLanguage: string = "";

async function navigateToSymbol(bufferId: number, sym: SymbolItem): Promise<void> {
  const bytePos = await editor.getLineStartPosition(sym.startLine);

  if (bytePos === null) return;

  editor.setBufferCursor(bufferId, bytePos);

  const lineCount = sym.endLine - sym.startLine + 1;

  if (lineCount > 1) {
    editor.executeActions([
      { action: "select_line", count: 1 },
      { action: "select_down", count: lineCount - 2 },
      { action: "select_line_end", count: 1 },
    ]);
  } else {
    editor.executeActions([{ action: "select_line_end", count: 1 }]);
  }

  editor.scrollBufferToLine(bufferId, sym.startLine);
}

async function loadSymbols(filePath: string, language: string): Promise<SymbolItem[]> {
  try {
    const uri = editor.pathToFileUri(filePath);
    const result = await editor.sendLspRequest(
      language,
      "textDocument/documentSymbol",
      {
        textDocument: { uri },
      },
    );

    const symbols = parseSymbols(result);

    if (symbols.length > 0 && cachedBufferId !== null) {
      await navigateToSymbol(cachedBufferId, symbols[0]);
    }

    return symbols;
  } catch (err) {
    const msg = err instanceof Error ? err.message : String(err);
    editor.setStatus(`LSP symbols failed: ${msg}`);
    return [];
  }
}

const finder = new Finder(editor, {
  id: "lsp_symbols",
  preview: false,
  format: (sym, i) => ({
    label: `[${getKindLabel(sym.kind)}] ${sym.name}`,
    description: `line ${sym.startLine + 1}`,
    location: { file: cachedFilePath, line: sym.startLine, column: 1 },
  }),
  onSelect: async (sym) => {
    if (cachedBufferId !== null) {
      await navigateToSymbol(cachedBufferId, sym);
    }
  },
  onSelectionChanged: async (sym) => {
    if (cachedBufferId !== null) {
      await navigateToSymbol(cachedBufferId, sym);
    }
  },
});

const finderSource: FilterSource<SymbolItem> = {
  mode: "filter",
  load: async () => loadSymbols(cachedFilePath, cachedLanguage),
  // Filter for callig navigateToSymbol() while typing - live
  // lsp symbols switching/selection.
  filter: (items, query) => {
    const filtered = defaultFuzzyFilter(
      items,
      query,
      (sym, i) => ({
        label: `[${getKindLabel(sym.kind)}] ${sym.name}`,
        description: `line ${sym.startLine + 1}`,
      }),
      100,
    );

    filtered.sort((a, b) => a.startLine - b.startLine);

    if (filtered.length > 0 && cachedBufferId !== null) {
      navigateToSymbol(cachedBufferId, filtered[0]);
    }

    return filtered;
  },
};

async function openSymbolsListHandler(): Promise<void> {
  cachedBufferId = editor.getActiveBufferId();

  if (cachedBufferId === null) {
    return;
  }

  cachedLanguage = editor.getBufferInfo(cachedBufferId)?.language;

  if (!cachedLanguage) {
    return;
  }

  cachedFilePath = editor.getBufferPath(cachedBufferId);

  if (!cachedFilePath) {
    return;
  }

  finder.prompt({
    title: "Go to symbol: ",
    source: finderSource,
  });
}

registerHandler("goto_lsp_symbol", openSymbolsListHandler);

function parseSymbols(result: unknown): SymbolItem[] {
  const symbols: SymbolItem[] = [];

  if (!result) return symbols;

  if (Array.isArray(result)) {
    for (const item of result) {
      if (typeof item !== "object" || item === null) continue;

      const raw = item as Record<string, unknown>;
      const kind = Number(raw.kind) || 0;
      const name = String(raw.name ?? "");

      if (!name) continue;

      let startLine = 1;
      let endLine = 1;

      if ("location" in raw && typeof raw.location === "object") {
        const loc = raw.location as Record<string, unknown>;

        if ("range" in loc && typeof loc.range === "object") {
          const range = loc.range as Record<string, unknown>;
          const start = range.start as Record<string, unknown>;
          const end = range.end as Record<string, unknown>;

          startLine = typeof start.line === "number" ? start.line : 0;
          endLine = typeof end.line === "number" ? end.line : startLine;
        }
      } else if ("selectionRange" in raw) {
        const selectionRange = raw.selectionRange as Record<string, unknown>;
        const start = selectionRange.start as Record<string, unknown>;
        const end = selectionRange.end as Record<string, unknown>;

        startLine = typeof start.line === "number" ? start.line : 0;
        endLine = typeof end.line === "number" ? end.line : startLine;
      }

      symbols.push({
        name,
        kind,
        startLine,
        endLine,
      });
    }
  }

  symbols.sort((a, b) => a.startLine - b.startLine);

  return symbols;
}

editor.registerCommand(
  "%cmd.goto_lsp_symbol",
  "%cmd.goto_lsp_symbol_desc",
  "goto_lsp_symbol",
);

editor.debug("LSP navigation plugin loaded");
