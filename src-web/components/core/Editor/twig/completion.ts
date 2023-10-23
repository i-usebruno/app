import type { CompletionContext } from '@codemirror/autocomplete';

const openTag = '${[ ';
const closeTag = ' ]}';

const variables: { name: string }[] = [
  { name: 'foo' }
];

const MIN_MATCH_VAR = 2;
const MIN_MATCH_NAME = 3;

export function completions(context: CompletionContext) {
  const toStartOfName = context.matchBefore(/\w*/);
  const toStartOfVariable = context.matchBefore(/\$\{?\[?\s*\w*/);
  const toMatch = toStartOfVariable ?? toStartOfName ?? null;

  if (toMatch === null) return null;

  const matchLen = toMatch.to - toMatch.from;

  const failedVarLen = toStartOfVariable !== null && matchLen < MIN_MATCH_VAR;
  if (failedVarLen && !context.explicit) {
    return null;
  }

  const failedNameLen = toStartOfVariable === null && matchLen < MIN_MATCH_NAME;
  if (failedNameLen && !context.explicit) {
    return null;
  }

  // TODO: Figure out how to make autocomplete stay open if opened explicitly. It sucks when you explicitly
  //  open it, then it closes when you type the next character.
  return {
    from: toMatch.from,
    options: variables
      .map((v) => ({
        label: toStartOfVariable ? `${openTag}${v.name}${closeTag}` : v.name,
        apply: `${openTag}${v.name}${closeTag}`,
        type: 'variable',
        matchLen,
      }))
      // Filter out exact matches
      .filter((o) => o.label !== toMatch.text),
  };
}
