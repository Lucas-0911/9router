import { Component, Suspense, useEffect, useState } from "react";
import { createRoot } from "react-dom/client";
import { ThemeProvider } from "@/shared/components/ThemeProvider";
import { RuntimeI18nProvider } from "@/i18n/RuntimeI18nProvider";
import DashboardLayout from "@/shared/components/layouts/DashboardLayout";
import EndpointPageClient from "@/app/(dashboard)/dashboard/endpoint/EndpointPageClient";
import CLIToolsPageClient from "@/app/(dashboard)/dashboard/cli-tools/CLIToolsPageClient";
import ToolDetailClient from "@/app/(dashboard)/dashboard/cli-tools/[toolId]/ToolDetailClient";
import { CLI_TOOLS } from "@/shared/constants/cliTools";
import { CardSkeleton } from "@/shared/components/Loading";
import { routes } from "./routes";
import { usePathname, useParams, navigate } from "./navigation";
import { connectBackend } from "./backend";

export function MachinePage({ tool = false }) {
  const { toolId } = useParams();
  const [machineId, setMachineId] = useState(null);
  const [error, setError] = useState(null);
  useEffect(() => {
    fetch("/api/machine-id").then(async (response) => {
      if (!response.ok) throw new Error("Unable to load machine identity");
      const data = await response.json();
      if (typeof data.machineId !== "string" || !data.machineId) throw new Error("Missing machine identity");
      setMachineId(data.machineId);
    }).catch(setError);
  }, []);
  if (error) throw error;
  if (!machineId) return <CardSkeleton />;
  if (!tool) return <EndpointPageClient machineId={machineId} />;
  if (toolId) {
    if (!CLI_TOOLS[toolId]) return <p>Page not found</p>;
    return <ToolDetailClient toolId={toolId} machineId={machineId} />;
  }
  return <CLIToolsPageClient machineId={machineId} />;
}

class PageError extends Component {
  state = { error: null };
  static getDerivedStateFromError(error) { return { error }; }
  render() {
    if (!this.state.error) return this.props.children;
    return <div role="alert" className="p-8"><h1 className="text-xl font-semibold mb-4">{this.state.error.message === "NEXT_NOT_FOUND" ? "Page not found" : "Unable to load this page"}</h1><p className="text-text-muted">{this.state.error.message === "NEXT_NOT_FOUND" ? "Check the address or return to the dashboard." : this.state.error.message}</p><button className="mt-4 text-primary" onClick={() => window.location.reload()}>Reload</button></div>;
  }
}
function App() {
  const pathname = usePathname();
  const [authorized, setAuthorized] = useState(false);
  const [authError, setAuthError] = useState(null);
  const dashboard = pathname.startsWith("/dashboard");
  useEffect(() => {
    if (pathname === "/" || pathname === "/index.html") { navigate("/dashboard", { replace: true }); return; }
    if (!dashboard) return;
    let active = true;
    fetch("/api/auth/status", { cache: "no-store" }).then(async (response) => {
      if (!response.ok) throw new Error("Unable to check dashboard login");
      const status = await response.json();
      if (!active) return;
      if (status.authenticated || status.requireLogin === false) setAuthorized(true);
      else navigate("/login", { replace: true });
    }).catch((error) => active && setAuthError(error));
    return () => { active = false; };
  }, [dashboard, pathname]);
  if (authError) throw authError;
  const route = routes.find(({ pattern }) => pattern.test(pathname));
  const Page = route?.page;
  const content = <PageError key={pathname}><Suspense fallback={<CardSkeleton />}>{Page ? <Page /> : <p className="p-8">Page not found</p>}</Suspense></PageError>;
  return <ThemeProvider><RuntimeI18nProvider>{dashboard ? authorized ? <DashboardLayout>{content}</DashboardLayout> : <CardSkeleton /> : content}</RuntimeI18nProvider></ThemeProvider>;
}

connectBackend().then(() => {
  createRoot(document.getElementById("root")).render(<PageError><App /></PageError>);
}).catch((error) => {
  const root = document.getElementById("root");
  root.replaceChildren();
  const message = document.createElement("p");
  message.setAttribute("role", "alert");
  message.textContent = `Unable to connect to the local Rust backend: ${error.message}`;
  root.append(message);
});
