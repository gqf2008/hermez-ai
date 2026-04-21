import { useMemo, useCallback } from 'react';

function parseInline(text: string): string {
  return text
    .replace(/\*\*(.+?)\*\*/g, '<strong>$1</strong>')
    .replace(/\*(.+?)\*/g, '<em>$1</em>')
    .replace(/`([^`]+)`/g, '<code>$1</code>')
    .replace(/\[(.+?)\]\((.+?)\)/g, '<a href="$2" target="_blank" rel="noopener">$1</a>')
    .replace(/(https?:\/\/[^\s<]+)/g, '<a href="$1" target="_blank" rel="noopener">$1</a>');
}

function CopyButton({ text }: { text: string }) {
  const handleCopy = useCallback(() => {
    navigator.clipboard.writeText(text).then(() => {
      /* user feedback handled by CSS active state */
    });
  }, [text]);

  return (
    <button className="copy-btn" onClick={handleCopy} title="Copy" aria-label="Copy code">
      <svg viewBox="0 0 16 16" width="14" height="14" fill="currentColor">
        <path d="M0 6.75C0 5.784.784 5 1.75 5h1.5a.75.75 0 010 1.5h-1.5a.25.25 0 00-.25.25v7.5c0 .138.112.25.25.25h7.5a.25.25 0 00.25-.25v-1.5a.75.75 0 011.5 0v1.5A1.75 1.75 0 019.25 16h-7.5A1.75 1.75 0 010 14.25v-7.5z" />
        <path d="M5 1.75C5 .784 5.784 0 6.75 0h7.5C15.216 0 16 .784 16 1.75v7.5A1.75 1.75 0 0114.25 11h-7.5A1.75 1.75 0 015 9.25v-7.5zm1.75-.25a.25.25 0 00-.25.25v7.5c0 .138.112.25.25.25h7.5a.25.25 0 00.25-.25v-7.5a.25.25 0 00-.25-.25h-7.5z" />
      </svg>
    </button>
  );
}

export default function Markdown({ text }: { text: string }) {
  const blocks = useMemo(() => {
    const lines = text.split('\n');
    const result: Array<{ type: string; content: string; lang?: string }> = [];
    let i = 0;

    while (i < lines.length) {
      const line = lines[i];

      // Code block
      if (line.startsWith('```')) {
        const lang = line.slice(3).trim();
        i++;
        const codeLines: string[] = [];
        while (i < lines.length && !lines[i].startsWith('```')) {
          codeLines.push(lines[i]);
          i++;
        }
        i++; // skip ```
        result.push({ type: 'code', content: codeLines.join('\n'), lang });
        continue;
      }

      // Blockquote
      if (line.startsWith('> ')) {
        const quoteLines: string[] = [];
        while (i < lines.length && lines[i].startsWith('> ')) {
          quoteLines.push(lines[i].slice(2));
          i++;
        }
        result.push({ type: 'blockquote', content: parseInline(quoteLines.join('\n').trim()) });
        continue;
      }

      // Heading
      const headingMatch = line.match(/^(#{1,6})\s+(.+)$/);
      if (headingMatch) {
        const level = headingMatch[1].length;
        result.push({ type: 'heading', content: `<h${level}>${parseInline(headingMatch[2])}</h${level}>` });
        i++;
        continue;
      }

      // List item
      if (line.match(/^[-*]\s/)) {
        const listItems: string[] = [];
        while (i < lines.length && lines[i].match(/^[-*]\s/)) {
          listItems.push(`<li>${parseInline(lines[i].replace(/^[-*]\s/, ''))}</li>`);
          i++;
        }
        result.push({ type: 'list', content: `<ul>${listItems.join('')}</ul>` });
        continue;
      }

      // Numbered list
      if (line.match(/^\d+\.\s/)) {
        const listItems: string[] = [];
        while (i < lines.length && lines[i].match(/^\d+\.\s/)) {
          listItems.push(`<li>${parseInline(lines[i].replace(/^\d+\.\s/, ''))}</li>`);
          i++;
        }
        result.push({ type: 'list', content: `<ol>${listItems.join('')}</ol>` });
        continue;
      }

      // Empty line
      if (line.trim() === '') {
        i++;
        continue;
      }

      // Paragraph
      const paraLines: string[] = [line];
      i++;
      while (i < lines.length && lines[i].trim() !== '' && !lines[i].startsWith('```') && !lines[i].match(/^(#{1,6})\s|^[-*]\s|^\d+\.\s|^>\s/)) {
        paraLines.push(lines[i]);
        i++;
      }
      result.push({ type: 'paragraph', content: `<p>${parseInline(paraLines.join(' '))}</p>` });
    }

    return result;
  }, [text]);

  return (
    <div className="markdown-body">
      {blocks.map((block, idx) => {
        if (block.type === 'code') {
          return (
            <div key={idx} className="code-block-wrapper">
              {block.lang && <div className="code-block-lang">{block.lang}</div>}
              <CopyButton text={block.content} />
              <pre className="code-block">
                <code className={block.lang ? `lang-${block.lang}` : undefined}>
                  {block.content}
                </code>
              </pre>
            </div>
          );
        }
        return <div key={idx} dangerouslySetInnerHTML={{ __html: block.content }} />;
      })}
    </div>
  );
}
