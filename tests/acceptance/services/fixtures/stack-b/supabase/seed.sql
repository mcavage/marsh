CREATE TABLE IF NOT EXISTS public.items (id serial PRIMARY KEY, name text NOT NULL, note text);
CREATE TABLE IF NOT EXISTS public.tokens (token text PRIMARY KEY, at timestamptz DEFAULT now());
INSERT INTO public.items (name, note) SELECT 'item-' || g, 'seed' FROM generate_series(1, 1000) g;
INSERT INTO storage.buckets (id, name, public) VALUES ('uploads', 'uploads', false) ON CONFLICT DO NOTHING;
