// Reads a recording back.
//
//     node tools/capture/summarize.js                    what happened, in order
//     node tools/capture/summarize.js ops                GraphQL operations, with doc_id
//     node tools/capture/summarize.js shape <needle> [n] the shape of the n-th reply whose
//                                                        friendly name or URL contains <needle>
//     node tools/capture/summarize.js profiles           the profile header of every account
//                                                        visited, one line each, for comparing
//
// The default listing is the navigations and a count per endpoint, which is
// enough to see which calls a page makes. `ops` names the Relay operations on
// /api/graphql and /graphql/query by their X-FB-Friendly-Name, which is how a
// query is told apart when every one is a POST to the same address. `shape`
// prints the key tree of one body with long strings cut, which is what a
// model in snob-ig is written against. `profiles` pulls the same fields out
// of every PolarisProfilePageContentQuery so accounts can be compared side
// by side -- private, followed, counts, mutuals, whether a story is up.
'use strict';

const fs = require('fs');
const path = require('path');

const LOG = path.join(__dirname, 'out', 'events.jsonl');

function* events() {
  const text = fs.readFileSync(LOG, 'utf8');
  for (const line of text.split('\n')) {
    if (line) yield JSON.parse(line);
  }
}

function friendlyName(event) {
  for (const [name, value] of Object.entries(event.requestHeaders || {})) {
    if (name.toLowerCase() === 'x-fb-friendly-name') return value;
  }
  return '';
}

function variables(event) {
  const params = new URLSearchParams(event.postData || '');
  const raw = params.get('variables');
  if (!raw) return null;
  try { return JSON.parse(raw); } catch { return raw; }
}

function docId(event) {
  return new URLSearchParams(event.postData || '').get('doc_id') || '';
}

function shape(value, depth = 0, lines = []) {
  const pad = '  '.repeat(depth);
  if (Array.isArray(value)) {
    lines.push(`${pad}[list of ${value.length}]`);
    if (value.length) shape(value[0], depth + 1, lines);
  } else if (value && typeof value === 'object') {
    for (const [key, inner] of Object.entries(value)) {
      if (inner && typeof inner === 'object') {
        lines.push(`${pad}${key}:`);
        shape(inner, depth + 1, lines);
      } else {
        lines.push(`${pad}${key}: ${JSON.stringify(inner).slice(0, 90)}`);
      }
    }
  } else {
    lines.push(`${pad}${JSON.stringify(value).slice(0, 90)}`);
  }
  return lines;
}

function overview() {
  const endpoints = new Map();
  const navs = [];
  let docs = 0;
  for (const e of events()) {
    if (e.kind === 'nav') navs.push(`${e.seq}\t${e.url}`);
    else if (e.kind === 'doc') docs++;
    else if (e.kind === 'api') {
      const key = `${e.method} ${new URL(e.url).pathname}`;
      endpoints.set(key, (endpoints.get(key) || 0) + 1);
    }
  }
  console.log(`documents: ${docs}\n\n--- navigations`);
  console.log(navs.join('\n'));
  console.log('\n--- api endpoints');
  for (const [key, count] of [...endpoints].sort((a, b) => b[1] - a[1])) {
    console.log(`${String(count).padStart(4)} ${key}`);
  }
}

function ops() {
  const seen = new Map();
  for (const e of events()) {
    if (e.kind !== 'api') continue;
    const p = new URL(e.url).pathname;
    if (p !== '/api/graphql' && p !== '/graphql/query') continue;
    const key = `${p} ${friendlyName(e) || '<no friendly name>'}`;
    const entry = seen.get(key) || { count: 0, docId: docId(e) };
    entry.count++;
    seen.set(key, entry);
  }
  for (const [key, { count, docId: id }] of [...seen].sort((a, b) => b[1].count - a[1].count)) {
    console.log(`${String(count).padStart(4)} ${key.padEnd(80)} doc_id=${id}`);
  }
}

function show(needle, which) {
  let n = 0;
  for (const e of events()) {
    if (e.kind !== 'api') continue;
    if (!friendlyName(e).includes(needle) && !e.url.includes(needle)) continue;
    if (++n !== which) continue;
    console.log(`=== seq ${e.seq} ${e.method} ${e.url.slice(0, 120)}\npage ${e.page}\nstatus ${e.status}`);
    const vars = variables(e);
    if (vars) console.log(`variables ${JSON.stringify(vars).slice(0, 300)}`);
    try {
      console.log(shape(JSON.parse(e.body)).join('\n'));
    } catch {
      console.log(`body is not JSON: ${String(e.body).slice(0, 300)}`);
    }
    return;
  }
  console.error(`no reply #${which} matching "${needle}"`);
  process.exit(1);
}

function profiles() {
  for (const e of events()) {
    if (e.kind !== 'api' || friendlyName(e) !== 'PolarisProfilePageContentQuery') continue;
    let user;
    try { user = JSON.parse(e.body).data.user; } catch { continue; }
    if (!user) continue;
    const f = user.friendship_status || {};
    console.log(JSON.stringify({
      username: user.username,
      pk: user.pk,
      private: user.is_private,
      verified: user.is_verified,
      business: user.is_business,
      followers: user.follower_count,
      following: user.following_count,
      posts: user.media_count,
      mutual: user.mutual_followers_count,
      you_follow: f.following,
      follows_you: f.followed_by,
      story_up: user.latest_reel_media,
      named: (user.profile_context_links_with_user_ids || []).map(x => x.username),
    }));
  }
}

const [mode, needle, which] = process.argv.slice(2);
if (!fs.existsSync(LOG)) {
  console.error(`nothing recorded at ${LOG}; run record.js first`);
  process.exit(1);
}
switch (mode) {
  case undefined: overview(); break;
  case 'ops': ops(); break;
  case 'shape': show(needle || '', Number(which || 1)); break;
  case 'profiles': profiles(); break;
  default:
    console.error('usage: summarize.js [ops | shape <needle> [n] | profiles]');
    process.exit(2);
}
