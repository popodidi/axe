import ReactMarkdown from "react-markdown";
import remarkGfm from "remark-gfm";
import rehypeSanitize from "rehype-sanitize";
import rehypeHighlight from "rehype-highlight";
import { CodeBlock, Prose, Table } from "../components/ui";
import { rehypeHeadingIds } from "../lib/doc-toc";
export default function Markdown({
  body,
  compact = false,
  headingIds,
}: {
  body: string;
  compact?: boolean;
  // Give h1-h3 anchor ids with this prefix (the doc page's contents).
  headingIds?: string;
}) {
  return (
    <Prose compact={compact}>
      <ReactMarkdown
        remarkPlugins={[remarkGfm]}
        rehypePlugins={[
          rehypeSanitize,
          rehypeHighlight,
          ...(headingIds === undefined
            ? []
            : [[rehypeHeadingIds, { prefix: headingIds }] as [typeof rehypeHeadingIds, object]]),
        ]}
        components={{
          // In-page anchors stay in the page; other links open a new tab.
          a: ({ children, node, ...props }) =>
            props.href?.startsWith("#") ? (
              <a {...props}>{children}</a>
            ) : (
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
