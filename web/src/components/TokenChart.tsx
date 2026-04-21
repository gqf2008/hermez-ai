import { useMemo } from 'react';

interface Message {
  role: string;
  content?: string;
  reasoning?: string;
}

function estimateTokens(text?: string): number {
  if (!text) return 0;
  // Rough estimate: ~4 chars per token for English, ~2 for CJK
  let tokens = 0;
  for (const ch of text) {
    tokens += (ch.charCodeAt(0) > 127) ? 0.5 : 0.25;
  }
  return Math.max(1, Math.round(tokens));
}

export default function TokenChart({ messages }: { messages: Message[] }) {
  const data = useMemo(() => {
    const items = messages.map((m, i) => {
      const contentTokens = estimateTokens(m.content);
      const reasoningTokens = estimateTokens(m.reasoning);
      return {
        index: i + 1,
        role: m.role,
        content: contentTokens,
        reasoning: reasoningTokens,
        total: contentTokens + reasoningTokens,
      };
    });
    const max = Math.max(1, ...items.map(d => d.total));
    return { items, max };
  }, [messages]);

  if (data.items.length === 0) return null;

  const totalTokens = data.items.reduce((s, d) => s + d.total, 0);

  return (
    <div className="token-chart">
      <h3>Token Estimates ({totalTokens.toLocaleString()} total)</h3>
      <div className="token-chart-bars">
        {data.items.map(d => {
          const h = Math.max(4, (d.total / data.max) * 100);
          return (
            <div key={d.index} className="token-bar-wrapper" title={`#${d.index} ${d.role}: ~${d.total} tokens`}>
              <div className="token-bar-stack" style={{ height: `${h}%` }}>
                {d.reasoning > 0 && (
                  <div className="token-bar-reasoning" style={{ flex: d.reasoning }} />
                )}
                <div className="token-bar-content" style={{ flex: d.content }} />
              </div>
              <span className="token-bar-label">{d.index}</span>
            </div>
          );
        })}
      </div>
      <div className="token-chart-legend">
        <span className="legend-item"><span className="legend-dot content" /> Content</span>
        <span className="legend-item"><span className="legend-dot reasoning" /> Reasoning</span>
      </div>
    </div>
  );
}
