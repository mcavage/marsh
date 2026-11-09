INSERT INTO storage.buckets (id, name, public) VALUES ('avatars', 'avatars', false) ON CONFLICT DO NOTHING;
