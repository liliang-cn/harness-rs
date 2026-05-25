import { LoginForm } from '@/components/login-form';

export function Login() {
  return (
    <div className="bg-muted/40 flex min-h-svh w-full items-center justify-center p-6 md:p-10">
      <div className="w-full max-w-sm">
        <LoginForm />
      </div>
    </div>
  );
}
