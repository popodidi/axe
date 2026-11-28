import type { ReactNode } from "react";
import { Inter } from "next/font/google";
import "./globals.css";
import { themeScript } from "../lib/theme-script";
import { layoutScript } from "../lib/layout-script";
// Axe OS type: next/font downloads Inter at build time and serves it from
// this app, so the browser makes no remote font request. --font-body in
// brand-tokens.css reads --font-inter. Icons: app/favicon.ico, icon.svg and
// apple-icon.png (Next.js file conventions).
const inter = Inter({ subsets: ["latin"], variable: "--font-inter", display: "swap" });
export const metadata = {
  title: "Axe OS · Tasks",
  description: "Read-only Axe OS task queue dashboard",
};
export default function Layout({ children }: { children: ReactNode }) {
  return (
    // The theme script sets data-theme before hydration.
    <html lang="en" className={inter.variable} suppressHydrationWarning>
      <head>
        {/* Applies a stored light theme before first paint (no flash). */}
        <script dangerouslySetInnerHTML={{ __html: themeScript }} />
        {/* Applies saved nav and detail-pane sizes before first paint. */}
        <script dangerouslySetInnerHTML={{ __html: layoutScript }} />
      </head>
      <body>{children}</body>
    </html>
  );
}
