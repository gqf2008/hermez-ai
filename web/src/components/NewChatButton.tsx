import { useState } from 'react';
import { useNavigate } from 'react-router-dom';
import { api, mockApi, safeApi } from '../api/client';

export default function NewChatButton() {
  const navigate = useNavigate();
  const [isCreating, setIsCreating] = useState(false);

  const handleClick = async () => {
    if (isCreating) return;
    setIsCreating(true);
    try {
      const res = await safeApi(() => api.createSession('New Chat'), () => mockApi.createSession('New Chat'));
      if (res.id) {
        navigate(`/sessions/${res.id}`);
      }
    } catch (e) {
      console.error('Failed to create session', e);
    } finally {
      setIsCreating(false);
    }
  };

  return (
    <button
      className="new-chat-btn"
      onClick={handleClick}
      disabled={isCreating}
      title="Start a new chat"
    >
      {isCreating ? '…' : '+ New Chat'}
    </button>
  );
}
