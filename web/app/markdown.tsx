import ReactMarkdown from "react-markdown";
import remarkGfm from "remark-gfm";
import rehypeSanitize from "rehype-sanitize";
import rehypeHighlight from "rehype-highlight";
import { CodeBlock, Prose, Table } from "../components/ui";
export default function Markdown({
  body,
  compact = false,
}: {
  body: string;
  compact?: boolean;
}) {
  return (
    <Prose compact={compact}>
      <ReactMarkdown
        remarkPlugins={[remarkGfm]}
        rehypePlugins={[rehypeSanitize, rehypeHighlight]}
        components={{
          a: ({ children, node, ...props }) => (
            <a {...props} target="_blank" rel="noopener noreferrer">
              {children}
            </a>
          ),
          table: ({ node, ...props }) => <Table {...props} />,
          pre: ({ node, ...props }) => <CodeBlock {...props} />,
        }}
      >
        {body}
      </ReactMarkdown>
    </Prose>
  );
}
