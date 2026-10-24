// Expressive Code plugin for `console` blocks: lines starting with `$ ` are
// commands, every other line is their output. Output lines render dimmed on a
// shaded background, and the copy button copies only the commands.
const CONSOLE_LANGUAGES = new Set(["console", "shellsession", "sh-session"]);
const PROMPT = "$ ";

function outputLines(codeBlock) {
  const output = new Set();
  let continued = false;
  codeBlock.getLines().forEach((line, index) => {
    const isCommand = continued || line.text.startsWith(PROMPT);
    if (!isCommand) {
      output.add(index);
    }
    continued = isCommand && line.text.trimEnd().endsWith("\\");
  });
  return output;
}

function commands(codeBlock) {
  const output = outputLines(codeBlock);
  return codeBlock
    .getLines()
    .filter((_, index) => !output.has(index))
    .map((line) => (line.text.startsWith(PROMPT) ? line.text.slice(PROMPT.length) : line.text))
    .join("\n");
}

function addClass(node, className) {
  const classes = node.properties.className ?? [];
  node.properties.className = [...classes, className];
}

function findCopyButton(node) {
  if (node.type === "element" && node.tagName === "button" && "dataCode" in (node.properties ?? {})) {
    return node;
  }
  for (const child of node.children ?? []) {
    const found = findCopyButton(child);
    if (found) {
      return found;
    }
  }
  return undefined;
}

export function consoleOutput() {
  return {
    name: "nyl-console-output",
    baseStyles: `
      .ec-line.output {
        background-color: color-mix(in srgb, currentColor 6%, transparent);
      }
      .ec-line.output .code {
        opacity: 0.6;
      }
    `,
    hooks: {
      postprocessRenderedLine: ({ codeBlock, line, renderData }) => {
        if (!CONSOLE_LANGUAGES.has(codeBlock.language)) {
          return;
        }
        const index = codeBlock.getLines().indexOf(line);
        if (outputLines(codeBlock).has(index)) {
          addClass(renderData.lineAst, "output");
        }
      },
      // Runs after every plugin rendered its blocks, so the frame's copy
      // button exists.
      postprocessRenderedBlockGroup: ({ renderedGroupContents }) => {
        for (const { codeBlock, renderedBlockAst } of renderedGroupContents) {
          if (!CONSOLE_LANGUAGES.has(codeBlock.language)) {
            continue;
          }
          const button = findCopyButton(renderedBlockAst);
          if (button) {
            button.properties.dataCode = commands(codeBlock).replace(/\n/g, "\x7F");
          }
        }
      },
    },
  };
}
