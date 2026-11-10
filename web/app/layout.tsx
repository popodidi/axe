import type { ReactNode } from "react";
import "./globals.css";
import { themeScript } from "../lib/theme-script";
import { layoutScript } from "../lib/layout-script";
export const metadata = {
  title: "Harvis · Tasks",
  description: "Read-only taskq workspace",
};
export default function Layout({ children }: { children: ReactNode }) {
  return (
    // The theme script sets data-theme before hydration.
    <html lang="en" suppressHydrationWarning>
      <head>
        {/* Applies a stored light theme before first paint (no flash). */}
        <script dangerouslySetInnerHTML={{ __html: themeScript }} />
        {/* Applies saved nav and detail-pane sizes before first paint. */}
        <script dangerouslySetInnerHTML={{ __html: layoutScript }} />
        {/* Quiet uses the native system font stack; no remote font load. */}
      </head>
      <body>{children}</body>
    </html>
  );
}
