'use strict';

const TOKEN_KEY = 'jarvo-office-token';
const LEFT_GRACE_MS = 60_000;

const banner = document.getElementById('banner');
const characters = document.getElementById('characters');
const roomEmpty = document.getElementById('room-empty');
const notice = document.getElementById('notice');
const panel = document.getElementById('panel');
const panelEmpty = document.getElementById('panel-empty');
const panelName = document.getElementById('panel-name');
const panelMeta = document.getElementById('panel-meta');
const panelSkills = document.getElementById('panel-skills');
const transcript = document.getElementById('transcript');
const composer = document.getElementById('composer');
const command = document.getElementById('command');
const spawnButton = document.getElementById('spawn');
const sendButton = composer.querySelector('button');

/** Agents by id: { info, status, tool, left }. */
const agents = new Map();
/** Agent cards by id, so a status change does not rebuild the room. */
const cards = new Map();
/** Forget timers for departed agents. */
const timers = new Map();

let socket = null;
let selectedId = null;
let awaitTail = false;
/** Live events seen while the tail is on its way: `[turn, seq, event]`. */
let buffered = [];
/** Highest sequence rendered for the selected agent. */
let lastSeq = 0;
/** The text node of the assistant line currently being appended to. */
let assistantText = null;

function showBanner(text) {
  banner.textContent = text;
  banner.hidden = false;
}

function showNotice(text) {
  notice.textContent = text;
  notice.hidden = false;
}

function clearNotice() {
  notice.textContent = '';
  notice.hidden = true;
}

/**
 * Read `?token=` once, move it to localStorage, and take it out of the
 * address bar. Never logged and never rendered.
 */
function takeToken() {
  const url = new URL(window.location.href);
  const fromUrl = url.searchParams.get('token');
  if (fromUrl) {
    try {
      localStorage.setItem(TOKEN_KEY, fromUrl);
    } catch (_) {
      // Storage may be denied; the connection still gets the token below.
    }
    url.searchParams.delete('token');
    const search = url.searchParams.toString();
    history.replaceState(null, '', `${url.pathname}${search ? `?${search}` : ''}${url.hash}`);
    return fromUrl;
  }
  try {
    return localStorage.getItem(TOKEN_KEY);
  } catch (_) {
    return null;
  }
}

function connect() {
  const token = takeToken();
  if (!token) {
    showBanner('No token. Open the link Jarvo printed, which ends with ?token=…');
    return;
  }

  const scheme = window.location.protocol === 'https:' ? 'wss' : 'ws';
  const url = `${scheme}://${window.location.host}/ws/browser?token=${encodeURIComponent(token)}`;
  socket = new WebSocket(url);

  socket.addEventListener('message', (event) => {
    let message;
    try {
      message = JSON.parse(event.data);
    } catch (_) {
      return;
    }
    handle(message);
  });
  socket.addEventListener('close', () => {
    showBanner('The office connection closed. Reload the page to reconnect.');
    setComposerEnabled(false);
  });
  socket.addEventListener('error', () => {
    showBanner('The office connection failed. Reload the page to try again.');
  });
}

function send(message) {
  if (!socket || socket.readyState !== WebSocket.OPEN) {
    showNotice('The office is not connected.');
    return false;
  }
  socket.send(JSON.stringify(message));
  return true;
}

function handle(message) {
  switch (message.type) {
    case 'roster':
      onRoster(message);
      break;
    case 'joined':
      onJoined(message);
      break;
    case 'left':
      onLeft(message);
      break;
    case 'turn':
      onTurn(message);
      break;
    case 'tail':
      onTail(message);
      break;
    case 'refused':
      onRefused(message);
      break;
    default:
      break;
  }
}

function onRoster(message) {
  for (const timer of timers.values()) clearTimeout(timer);
  timers.clear();
  agents.clear();
  cards.clear();
  characters.textContent = '';

  for (const info of message.agents) {
    agents.set(info.id, { info, status: 'idle', tool: null, left: false });
  }
  for (const agent of agents.values()) renderCard(agent);
  updateEmptyRoom();

  if (selectedId !== null && !agents.has(selectedId)) showEmptyPanel();
}

function onJoined(message) {
  const id = message.info.id;
  if (timers.has(id)) {
    clearTimeout(timers.get(id));
    timers.delete(id);
  }

  const prior = agents.get(id);
  const wasLeft = prior ? prior.left : false;
  const agent = prior || { info: message.info, status: 'idle', tool: null, left: false };
  agent.info = message.info;
  agent.left = false;
  if (wasLeft) {
    agent.status = 'idle';
    agent.tool = null;
  }
  agents.set(id, agent);
  renderCard(agent);
  updateEmptyRoom();
  if (id === selectedId) renderPanel(agent);
}

function onLeft(message) {
  const agent = agents.get(message.id);
  if (!agent) return;
  agent.left = true;
  renderCard(agent);

  if (timers.has(message.id)) clearTimeout(timers.get(message.id));
  timers.set(
    message.id,
    setTimeout(() => removeAgent(message.id), LEFT_GRACE_MS),
  );
}

function removeAgent(id) {
  timers.delete(id);
  const agent = agents.get(id);
  // A Joined after Left cancelled the timer; this is only a guard.
  if (agent && !agent.left) return;

  agents.delete(id);
  const card = cards.get(id);
  if (card) card.remove();
  cards.delete(id);
  updateEmptyRoom();
  if (selectedId === id) showEmptyPanel();
}

function updateEmptyRoom() {
  roomEmpty.hidden = agents.size > 0;
}

function renderCard(agent) {
  let card = cards.get(agent.info.id);
  if (!card) {
    card = document.createElement('button');
    card.type = 'button';
    card.className = 'character';
    card.addEventListener('click', () => selectAgent(agent.info.id));

    const dot = document.createElement('span');
    dot.className = 'dot';
    dot.dataset.role = 'dot';
    const name = document.createElement('span');
    name.className = 'name';
    name.dataset.role = 'name';
    const state = document.createElement('span');
    state.className = 'state';
    state.dataset.role = 'state';

    card.append(dot, name, state);
    cards.set(agent.info.id, card);
    characters.append(card);
  }

  card.querySelector('[data-role="name"]').textContent = agent.info.name;
  card.title = agent.info.cwd;
  card.classList.toggle('left', agent.left);
  card.classList.toggle('selected', selectedId === agent.info.id);
  card.setAttribute('aria-pressed', selectedId === agent.info.id ? 'true' : 'false');

  const dot = card.querySelector('[data-role="dot"]');
  const state = card.querySelector('[data-role="state"]');
  if (agent.left) {
    dot.className = 'dot left';
    state.textContent = 'left';
  } else {
    dot.className = `dot ${agent.status}`;
    state.textContent =
      agent.status === 'running' && agent.tool
        ? `running ${agent.tool}`
        : agent.status;
  }
}

function selectAgent(id) {
  const agent = agents.get(id);
  if (!agent) return;

  selectedId = id;
  awaitTail = true;
  buffered = [];
  lastSeq = 0;
  clearTranscript();
  renderPanel(agent);
  for (const other of agents.values()) renderCard(other);
  showNotice(`Loading ${agent.info.name}…`);

  if (send({ type: 'select', id })) {
    clearNotice();
    appendLine('system', 'loading transcript…');
  } else {
    awaitTail = false;
  }
}

function renderPanel(agent) {
  panel.hidden = false;
  panelEmpty.hidden = true;
  panelName.textContent = agent.info.name;
  panelMeta.textContent = `${agent.info.model} · ${agent.info.cwd}`;

  panelSkills.textContent = '';
  const skills = agent.info.skills || [];
  if (skills.length > 0) {
    panelSkills.append('Skills: ');
    skills.forEach((skill, index) => {
      if (index > 0) panelSkills.append(' ');
      const item = document.createElement('span');
      item.textContent = `/${skill}`;
      panelSkills.append(item);
    });
  }

  setComposerEnabled(!agent.left);
}

function showEmptyPanel() {
  selectedId = null;
  awaitTail = false;
  buffered = [];
  lastSeq = 0;
  clearTranscript();
  panel.hidden = true;
  panelEmpty.hidden = false;
  setComposerEnabled(false);
  for (const agent of agents.values()) renderCard(agent);
}

function setComposerEnabled(enabled) {
  command.disabled = !enabled;
  sendButton.disabled = !enabled;
  if (enabled) command.focus();
}

function onTurn(message) {
  const id = message.id;
  if (!agents.has(id)) return;

  applyStatus(id, message.event);
  if (id !== selectedId) return;

  if (awaitTail) {
    buffered.push([message.turn, message.seq, message.event]);
    return;
  }

  if (message.seq <= lastSeq) return;
  lastSeq = message.seq;
  renderEvent(message.event);
  transcript.scrollTop = transcript.scrollHeight;
}

function onTail(message) {
  if (message.id !== selectedId || !awaitTail) return;
  awaitTail = false;

  clearTranscript();
  let status = 'idle';
  let tool = null;
  lastSeq = 0;
  for (const [, seq, event] of message.events) {
    renderEvent(event);
    lastSeq = seq;
    const next = statusOf(event);
    if (next) [status, tool] = [next.status, next.tool];
  }

  // Everything at or below the tail's last sequence is already rendered;
  // a buffered event above it is what the snapshot missed.
  for (const [, seq, event] of buffered) {
    if (seq <= lastSeq) continue;
    renderEvent(event);
    lastSeq = seq;
    const next = statusOf(event);
    if (next) [status, tool] = [next.status, next.tool];
  }
  buffered = [];

  const agent = agents.get(selectedId);
  if (agent) {
    agent.status = status;
    agent.tool = tool;
    renderCard(agent);
  }
  transcript.scrollTop = transcript.scrollHeight;
}

function onRefused(message) {
  const line = `refused: ${message.reason}`;
  if (message.id === selectedId) appendLine('error', line);
  else showNotice(line);
}

function statusOf(event) {
  switch (event.type) {
    case 'started':
    case 'delta':
    case 'tool_result':
      return { status: 'thinking', tool: null };
    case 'tool_call':
      return { status: 'running', tool: event.name };
    case 'queued':
      return { status: 'waiting', tool: null };
    case 'completed':
    case 'failed':
      return { status: 'idle', tool: null };
    default:
      return null;
  }
}

function applyStatus(id, event) {
  const agent = agents.get(id);
  if (!agent) return;
  const next = statusOf(event);
  if (!next) return;
  agent.status = next.status;
  agent.tool = next.tool;
  renderCard(agent);
}

function clearTranscript() {
  transcript.textContent = '';
  assistantText = null;
}

function appendLine(kind, text) {
  const line = document.createElement('div');
  line.className = `line ${kind}`;
  line.textContent = text;
  transcript.append(line);
  transcript.scrollTop = transcript.scrollHeight;
  return line;
}

function ensureAssistant() {
  if (assistantText) return assistantText;
  const line = document.createElement('div');
  line.className = 'line assistant';
  assistantText = document.createTextNode('');
  line.append(assistantText);
  transcript.append(line);
  return assistantText;
}

function renderEvent(event) {
  switch (event.type) {
    case 'started':
      appendLine('user', event.prompt);
      assistantText = null;
      break;
    case 'delta':
      ensureAssistant().appendData(event.text);
      break;
    case 'tool_call':
      appendLine('system', `tool call: ${event.name}`);
      assistantText = null;
      break;
    case 'tool_result':
      appendLine('system', `tool result: ${event.name}`);
      assistantText = null;
      break;
    case 'notice':
      appendLine('system', event.text);
      break;
    case 'queued':
      appendLine('queued', `queued: ${event.text}`);
      break;
    case 'completed':
      if (event.text !== '') ensureAssistant().data = event.text;
      assistantText = null;
      break;
    case 'failed':
      appendLine('error', event.message);
      assistantText = null;
      break;
    default:
      break;
  }
}

composer.addEventListener('submit', (event) => {
  event.preventDefault();
  if (selectedId === null) return;

  const agent = agents.get(selectedId);
  if (!agent || agent.left) {
    appendLine('error', 'this agent has left the room');
    return;
  }

  const text = command.value;
  if (text === '') return;
  if (send({ type: 'send', id: selectedId, text })) command.value = '';
});

spawnButton.addEventListener('click', () => {
  const cwd = window.prompt('Directory for the new agent:');
  if (cwd === null) return;
  const name = window.prompt('Name (optional):');
  send({
    type: 'spawn',
    cwd,
    name: name === null || name === '' ? null : name,
  });
});

connect();
