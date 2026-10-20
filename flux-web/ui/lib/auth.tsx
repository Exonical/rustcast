"use client";

import { createContext, useContext, useEffect, useState, type ReactNode } from "react";
import { fetchMe, type Me } from "./api";

export interface AuthState {
  me: Me | null;
  loading: boolean;
}

const AuthContext = createContext<AuthState>({ me: null, loading: true });

export function AuthProvider({ children }: { children: ReactNode }) {
  const [state, setState] = useState<AuthState>({ me: null, loading: true });
  useEffect(() => {
    let cancelled = false;
    fetchMe()
      .then((me) => { if (!cancelled) setState({ me, loading: false }); })
      .catch(() => { if (!cancelled) setState({ me: null, loading: false }); });
    return () => { cancelled = true; };
  }, []);
  return <AuthContext.Provider value={state}>{children}</AuthContext.Provider>;
}

export function useAuth(): AuthState {
  return useContext(AuthContext);
}
