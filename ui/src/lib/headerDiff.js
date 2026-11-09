// A recorded exchange's headers on one side of the proxy against the other:
// what the client sent against what the backend got, and the backend's
// answer against what reached the client. Lined up the way a code diff is,
// longest common subsequence first, so a header the proxy rewrote reads as
// its old line followed by its new one.
//
// `before` and `after` are `[name, value]` lists. Each line comes back as
// `{ name, value, op }`, `op` being 'same', 'del' or 'add'.
export function headerDiff(before, after) {
  const key = ([name, value]) => `${name}\n${value}`;
  const a = before.map(key);
  const b = after.map(key);

  // lcs[i][j]: the longest common run of a[i..] and b[j..].
  const lcs = Array.from({ length: a.length + 1 }, () => new Array(b.length + 1).fill(0));
  for (let i = a.length - 1; i >= 0; i--) {
    for (let j = b.length - 1; j >= 0; j--) {
      lcs[i][j] = a[i] === b[j] ? lcs[i + 1][j + 1] + 1 : Math.max(lcs[i + 1][j], lcs[i][j + 1]);
    }
  }

  const lines = [];
  const line = ([name, value], op) => lines.push({ name, value, op });
  let i = 0;
  let j = 0;
  while (i < a.length && j < b.length) {
    if (a[i] === b[j]) {
      line(before[i], 'same');
      i++;
      j++;
    } else if (lcs[i + 1][j] >= lcs[i][j + 1]) {
      line(before[i++], 'del');
    } else {
      line(after[j++], 'add');
    }
  }
  while (i < a.length) line(before[i++], 'del');
  while (j < b.length) line(after[j++], 'add');
  return lines;
}
