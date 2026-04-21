import { useEffect, useState, useRef } from 'react';
import { useParams, useNavigate, Link } from 'react-router-dom';
import type { SessionDetail as SessionDetailType } from '../types';
import { api, mockApi, safeApi } from '../api/client';
import Markdown from '../components/Markdown';
import TokenChart from '../components/TokenChart';

function fmtTokens(n: number): string {
  if (n >= 1e6) return `${(n / 1e6).toFixed(1)}M`;
  if (n >= 1e3) return `${(n / 1e3).toFixed(1)}K`;
  return `${n}`;
}

export default function SessionDetail() {
  const { id } = useParams<{ id: string }>();
  const navigate = useNavigate();
  const [data, setData] = useState<SessionDetailType | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [isRenaming, setIsRenaming] = useState(false);
  const [newTitle, setNewTitle] = useState('');
  const [input, setInput] = useState('');
  const [isSending, setIsSending] = useState(false);
  const messagesEndRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    if (!id) return;
    safeApi(() => api.getSession(id), () => mockApi.getSession(id))
      .then(setData)
      .catch((e: Error) => setError(e.message))
      .finally(() => setLoading(false));
  }, [id]);

  const handleExport = async () => {
    if (!id) return;
    try {
      const blob = await api.exportSession(id);
      const url = URL.createObjectURL(blob);
      const a = document.createElement('a');
      a.href = url;
      a.download = `session-${id}.json`;
      a.click();
      URL.revokeObjectURL(url);
    } catch {
      alert('Failed to export session');
    }
  };

  const handleDelete = async () => {
    if (!id || !window.confirm('Delete this session permanently?')) return;
    try {
      await api.deleteSession(id);
      navigate('/sessions');
    } catch (e) {
      alert('Failed to delete session');
    }
  };

  const handleRename = async () => {
    if (!id || !newTitle.trim()) return;
    try {
      await api.renameSession(id, newTitle.trim());
      setIsRenaming(false);
      if (data && data.session) {
        setData({ ...data, session: { ...data.session, title: newTitle.trim() } });
      }
    } catch {
      alert('Failed to rename session');
    }
  };

  const handleSend = async () => {
    if (!id || !input.trim() || isSending) return;
    const userMsg = input.trim();
    setInput('');
    setIsSending(true);

    // Optimistically add user message + empty assistant placeholder
    setData(prev => {
      if (!prev) return prev;
      return {
        ...prev,
        messages: [
          ...prev.messages,
          { role: 'user', content: userMsg },
          { role: 'assistant', content: '' },
        ],
      };
    });

    try {
      let streamed = '';
      await api.chatStream(
        id,
        userMsg,
        undefined,
        (delta) => {
          streamed += delta;
          setData(prev => {
            if (!prev) return prev;
            const msgs = [...prev.messages];
            const last = msgs[msgs.length - 1];
            if (last && last.role === 'assistant') {
              msgs[msgs.length - 1] = { ...last, content: streamed };
            }
            return { ...prev, messages: msgs };
          });
        },
        (result) => {
          setData(prev => {
            if (!prev) return prev;
            const msgs = [...prev.messages];
            const last = msgs[msgs.length - 1];
            if (last && last.role === 'assistant') {
              msgs[msgs.length - 1] = { ...last, content: result.response };
            }
            return { ...prev, messages: msgs };
          });
          setIsSending(false);
        },
        (error) => {
          setData(prev => {
            if (!prev) return prev;
            const msgs = [...prev.messages];
            const last = msgs[msgs.length - 1];
            if (last && last.role === 'assistant') {
              msgs[msgs.length - 1] = { ...last, content: `Error: ${error}` };
            }
            return { ...prev, messages: msgs };
          });
          setIsSending(false);
        },
      );
    } catch (e) {
      setData(prev => {
        if (!prev) return prev;
        const msgs = [...prev.messages];
        const last = msgs[msgs.length - 1];
        if (last && last.role === 'assistant') {
          msgs[msgs.length - 1] = { ...last, content: `Error: ${String(e)}` };
        }
        return { ...prev, messages: msgs };
      });
      setIsSending(false);
    }
  };

  useEffect(() => {
    messagesEndRef.current?.scrollIntoView({ behavior: 'smooth' });
  }, [data?.messages.length]);

  if (loading) return <div className="loading">Loading…</div>;
  if (error) return <div className="error">{error}</div>;
  if (!data || !data.session) return <div className="error">Session not found</div>;

  const s = data.session;

  return (
    <div className="page">
      <div className="breadcrumb">
        <Link to="/sessions">← Sessions</Link>
      </div>

      <div className="session-header">
        {isRenaming ? (
          <div className="rename-row">
            <input
              className="search"
              value={newTitle}
              onChange={e => setNewTitle(e.target.value)}
              placeholder="New title"
              autoFocus
              onKeyDown={e => { if (e.key === 'Enter') handleRename(); }}
            />
            <button className="btn primary" onClick={handleRename}>Save</button>
            <button className="btn" onClick={() => setIsRenaming(false)}>Cancel</button>
          </div>
        ) : (
          <h1 onDoubleClick={() => { setIsRenaming(true); setNewTitle(s.title || ''); }} title="Double-click to rename">
            {s.title || 'Untitled Session'}
          </h1>
        )}
        <div className="session-actions">
          <button className="btn" onClick={handleExport}>Export JSON</button>
          <button className="btn danger" onClick={handleDelete}>Delete</button>
        </div>
      </div>

      <div className="session-meta">
        <span className="badge platform-{s.platform}">{s.platform}</span>
        <span className="badge">{s.model}</span>
        <span>Created: {new Date(s.created_at).toLocaleString()}</span>
        <span>Input: {fmtTokens(s.input_tokens)}</span>
        <span>Output: {fmtTokens(s.output_tokens)}</span>
        {s.cache_read_tokens + s.cache_write_tokens > 0 && (
          <span>Cache: {fmtTokens(s.cache_read_tokens + s.cache_write_tokens)}</span>
        )}
      </div>

      <TokenChart messages={data.messages} />
      <h2>Messages ({data.messages.length})</h2>
      <div className="message-list">
        {data.messages.map((m, i) => (
          <div key={i} className={`message message-${m.role}`}>
            <div className="message-header">
              <span className="message-role">{m.role}</span>
              {m.tool_name && <span className="badge">{m.tool_name}</span>}
            </div>
            <div className="message-body">
              {m.content ? (
                <Markdown text={m.content} />
              ) : m.tool_calls ? (
                <pre className="message-content">{m.tool_calls}</pre>
              ) : (
                <em className="message-empty">(no content)</em>
              )}
            </div>
            {m.reasoning && (
              <details className="message-reasoning">
                <summary>Reasoning</summary>
                <pre>{m.reasoning}</pre>
              </details>
            )}
          </div>
        ))}
        {isSending && (
          <div className="message message-assistant">
            <div className="message-header">
              <span className="message-role">assistant</span>
            </div>
            <div className="message-body">
              <em className="message-empty">Thinking…</em>
            </div>
          </div>
        )}
        <div ref={messagesEndRef} />
      </div>

      <div className="chat-input-bar">
        <textarea
          className="chat-input"
          placeholder="Type a message…"
          value={input}
          onChange={e => setInput(e.target.value)}
          onKeyDown={e => {
            if (e.key === 'Enter' && !e.shiftKey) {
              e.preventDefault();
              handleSend();
            }
          }}
          rows={2}
        />
        <button className="btn primary" onClick={handleSend} disabled={isSending || !input.trim()}>
          Send
        </button>
      </div>
    </div>
  );
}
